param(
 [string]$InstallDir='C:\ProgramData\MoonlightClipboard',
 [string]$DesktopUser,
 [string]$Bind,
 [string]$WebServer,
 [int]$Port=47991,
 [string]$Compiler='C:\Windows\Microsoft.NET\Framework64\v4.0.30319\csc.exe',
 [switch]$KeepStaging,
 # Test seams. Both default to the real machine; the failure-injection suite in
 # tests/install.Tests.ps1 supplies a simulated machine instead.
 [hashtable]$Operations=$null,
 [string]$BuildRoot=$null
)
$ErrorActionPreference='Stop'
$Task='MoonlightClipboard'
$Firewall='MoonlightClipboard-WebServer-Only'
$ScriptPath=$PSCommandPath
if([string]::IsNullOrEmpty($BuildRoot)){$BuildRoot=Split-Path -Parent $ScriptPath}
# Every file the running helper needs; a partially switched set is the failure
# mode this installer must not leave behind.
$Artifacts=@('MoonlightClipboard.exe','Core.dll','ClipboardGuard.exe')
# Build output used only by the fake-backend test pass, never installed.
$TestArtifacts=@('Tests.exe')
$TokenPattern='^[0-9a-f]{64}$'

# Task-scheduler durations are ISO-8601 durations ('PT1M', 'PT0S' - the format
# the Task Scheduler XML schema itself uses), which TimeSpan.Parse rejects.
# Accept both ISO-8601 and the TimeSpan round-trip format so a definition can
# be replayed verbatim into New-ScheduledTaskSettingsSet without the caller
# having to know which one the adapter happens to want.
function ConvertTo-TaskDuration {
 param([string]$Value,[string]$Field='duration')
 if([string]::IsNullOrWhiteSpace($Value)){return $null}
 $trimmed=$Value.Trim()
 try{
  if($trimmed -match '^P'){return [System.Xml.XmlConvert]::ToTimeSpan($trimmed)}
  return [TimeSpan]::Parse($trimmed,[System.Globalization.CultureInfo]::InvariantCulture)
 }catch{
  throw ($Field + ' is not a valid task-scheduler duration: "' + $Value + '". Use ISO-8601 (for example PT1M) or a TimeSpan value.')
 }
}

# ---------------------------------------------------------------------------
# Native command boundary.
#
# Every side effect on the machine (build, file, ACL, task, firewall, process,
# listener, clock) goes through one named scriptblock in this table. The
# installer never calls a native cmdlet directly, so a test can replace an
# entry to inject a failure at exactly one step and record the call order,
# without touching real scheduled tasks, the real firewall, real processes or
# the clipboard. Each scriptblock takes (arguments, context).
# ---------------------------------------------------------------------------
function New-DefaultOperations {
 return @{
  'ResolveSid' = { param($a,$c)
   ([System.Security.Principal.NTAccount]$a.User).Translate([System.Security.Principal.SecurityIdentifier]).Value }

  'CompilerExists' = { param($a,$c) (Test-Path -LiteralPath $a.Path -PathType Leaf) }

  'Compile' = { param($a,$c)
   $arguments=@('/nologo','/warnaserror+',('/target:' + $a.Target),('/out:' + $a.Output))
   foreach($reference in $a.References){$arguments+=('/r:' + $reference)}
   foreach($source in $a.Sources){$arguments+=$source}
   Push-Location -LiteralPath $a.WorkingDirectory
   try{& $a.Compiler @arguments; $code=$LASTEXITCODE}finally{Pop-Location}
   if($code -ne 0){throw ($a.ErrorMessage + ' (csc exit ' + $code + ')')}
   if(-not (Test-Path -LiteralPath $a.Output)){throw ($a.ErrorMessage + ': csc produced no output')} }

  'RunRegression' = { param($a,$c)
   Push-Location -LiteralPath $a.WorkingDirectory
   try{& $a.Executable; $code=$LASTEXITCODE}finally{Pop-Location}
   if($code -ne 0){throw ('Fake-backend regression tests failed (exit ' + $code + '); the running helper was not stopped')} }

  'EnsureDirectory' = { param($a,$c) New-Item -ItemType Directory -Force -Path $a.Path | Out-Null }

  # Delete a directory this run created itself. Only the uniquely named
  # .staging-<stamp>/.rollback-<stamp> directories qualify: everything inside
  # them was written by this process, so a recursive delete cannot destroy
  # unknown content. Any other path is refused rather than force-deleted.
  'RemoveOwnedDirectory' = { param($a,$c)
   if(-not (Test-Path -LiteralPath $a.Path)){return}
   $leaf=Split-Path -Leaf $a.Path
   if($leaf -notmatch '^\.(staging|rollback)-[0-9]{8}-[0-9]{6}$'){
    throw ('Refusing to recursively delete a directory this run does not own: ' + $a.Path)
   }
   Remove-Item -LiteralPath $a.Path -Recurse -Force -ErrorAction Stop }

  # Remove a directory only when it is empty. A recursive/forced delete of the
  # install directory would destroy content this run never created (a
  # concurrently written file, an operator's log), which the rollback contract
  # explicitly promises to keep. Returns $true only when the directory is gone.
  'RemoveDirectoryIfEmpty' = { param($a,$c)
   if(-not (Test-Path -LiteralPath $a.Path)){return $true}
   $children=@(Get-ChildItem -LiteralPath $a.Path -Force -ErrorAction Stop)
   if($children.Count -gt 0){return $false}
   Remove-Item -LiteralPath $a.Path -Force -ErrorAction Stop
   return (-not (Test-Path -LiteralPath $a.Path)) }

  'FileExists' = { param($a,$c) (Test-Path -LiteralPath $a.Path -PathType Leaf) }

  'DirectoryExists' = { param($a,$c) (Test-Path -LiteralPath $a.Path -PathType Container) }

  'CopyFile' = { param($a,$c) Copy-Item -LiteralPath $a.Source -Destination $a.Destination -Force }

  'RemoveFile' = { param($a,$c)
   if(Test-Path -LiteralPath $a.Path){Remove-Item -LiteralPath $a.Path -Force} }

  'ReadFile' = { param($a,$c)
   if(-not (Test-Path -LiteralPath $a.Path -PathType Leaf)){return $null}
   Get-Content -Raw -LiteralPath $a.Path }

  'WriteFile' = { param($a,$c)
   Set-Content -LiteralPath $a.Path -Value $a.Content -Encoding UTF8 }

  'GetFileHash' = { param($a,$c)
   if(-not (Test-Path -LiteralPath $a.Path -PathType Leaf)){return $null}
   (Get-FileHash -LiteralPath $a.Path -Algorithm SHA256).Hash.ToLowerInvariant() }

  'GetDirectoryAcl' = { param($a,$c)
   if(-not (Test-Path -LiteralPath $a.Path)){return $null}
   (Get-Acl -LiteralPath $a.Path).Sddl }

  # $a.Sid is supplied for a fresh restrictive ACL (inheritance removed, only
  # the desktop user, SYSTEM and Administrators get full control).
  'SetDirectoryAcl' = { param($a,$c)
   New-Item -ItemType Directory -Force -Path $a.Path | Out-Null
   $acl=New-Object System.Security.AccessControl.DirectorySecurity
   $acl.SetAccessRuleProtection($true,$false)
   foreach($identity in @($a.Sid,'S-1-5-18','S-1-5-32-544')){
    $translator=if($identity -is [string]){[System.Security.Principal.SecurityIdentifier]$identity}else{$identity}
    $rule=New-Object System.Security.AccessControl.FileSystemAccessRule($translator,'FullControl','ContainerInherit,ObjectInherit','None','Allow')
    $acl.AddAccessRule($rule)
   }
   Set-Acl -LiteralPath $a.Path -AclObject $acl }

  'RestoreDirectoryAcl' = { param($a,$c)
   $acl=New-Object System.Security.AccessControl.DirectorySecurity
   $acl.SetSecurityDescriptorSddlForm($a.Sddl)
   Set-Acl -LiteralPath $a.Path -AclObject $acl }

  'GetTask' = { param($a,$c)
   $existing=Get-ScheduledTask -TaskName $a.Name -ErrorAction SilentlyContinue
   if(-not $existing){return $null}
   $xml=$null
   try{$xml=Export-ScheduledTask -TaskName $a.Name -ErrorAction Stop}catch{$xml=$null}
   [pscustomobject]@{Name=$existing.TaskName;State=[string]$existing.State;Xml=$xml} }

  'RegisterTask' = { param($a,$c)
   if($a.Xml){
    Register-ScheduledTask -TaskName $a.Name -Xml $a.Xml -Force -ErrorAction Stop | Out-Null
   }else{
    $definition=$a.Definition
    $actions=@()
    foreach($item in @($definition.Action)){
     $arguments=@{Execute=$item.Execute}
     if($item.ContainsKey('Argument')){$arguments['Argument']=$item.Argument}
     if($item.ContainsKey('WorkingDirectory')){$arguments['WorkingDirectory']=$item.WorkingDirectory}
     $actions+=New-ScheduledTaskAction @arguments
    }
    $principal=$definition.Principal
    $triggerSettings=@{}
    if($definition.Trigger -and $definition.Trigger.Count -gt 0){
     $triggerSettings['Trigger']=@(foreach($item in @($definition.Trigger)){New-ScheduledTaskTrigger -AtLogOn -User $item.User})
    }
    $settings=@{}
    if($definition.Settings){
     foreach($key in $definition.Settings.Keys){$settings[$key]=$definition.Settings[$key]}
    }
    $taskArguments=@{TaskName=$a.Name;Action=$actions;Principal=(New-ScheduledTaskPrincipal @principal);Force=$true}
    if($triggerSettings.ContainsKey('Trigger')){$taskArguments['Trigger']=$triggerSettings['Trigger']}
    if($settings.Count -gt 0){
     # Durations stay in the definition's own ISO-8601 form and are converted
     # exactly once, here, at the boundary with the native cmdlet.
     $limit=if($settings.ContainsKey('ExecutionTimeLimit')){ConvertTo-TaskDuration ([string]$settings['ExecutionTimeLimit']) 'ExecutionTimeLimit'}else{$null}
     if($null -eq $limit){$limit=[TimeSpan]::Zero}
     $interval=$null
     if($settings.ContainsKey('RestartInterval')){
      $interval=ConvertTo-TaskDuration ([string]$settings['RestartInterval']) 'RestartInterval'
      # A restart policy that cannot be expressed is a definition error, not a
      # reason to silently install a task without supervision.
      if($null -eq $interval -or $interval -le [TimeSpan]::Zero){throw ('RestartInterval is not a positive duration: ' + [string]$settings['RestartInterval'])}
     }
     $count=if($settings.ContainsKey('RestartCount')){[int]$settings['RestartCount']}else{999}
     $multiple=if($settings.ContainsKey('MultipleInstances')){[string]$settings['MultipleInstances']}else{'IgnoreNew'}
     $startWhenAvailable=if($settings.ContainsKey('StartWhenAvailable')){[bool]$settings['StartWhenAvailable']}else{$true}
     $onBatteries=if($settings.ContainsKey('AllowStartOnBatteries')){[bool]$settings['AllowStartOnBatteries']}else{$true}
     $offBatteries=if($settings.ContainsKey('DontStopOnBatteries')){[bool]$settings['DontStopOnBatteries']}else{$true}
     $setArguments=@{
      ExecutionTimeLimit=$limit
      RestartCount=$count
      MultipleInstances=$multiple
      StartWhenAvailable=$startWhenAvailable
      AllowStartIfOnBatteries=$onBatteries
      DontStopIfGoingOnBatteries=$offBatteries
     }
     if($null -ne $interval){$setArguments['RestartInterval']=$interval}
     $taskArguments['Settings']=New-ScheduledTaskSettingsSet @setArguments
    }
    Register-ScheduledTask @taskArguments -ErrorAction Stop | Out-Null
   } }

  'UnregisterTask' = { param($a,$c)
   if(Get-ScheduledTask -TaskName $a.Name -ErrorAction SilentlyContinue){
    Unregister-ScheduledTask -TaskName $a.Name -Confirm:$false -ErrorAction Stop } }

  'StartTask' = { param($a,$c) Start-ScheduledTask -TaskName $a.Name -ErrorAction Stop }

  'StopTask' = { param($a,$c)
   if(Get-ScheduledTask -TaskName $a.Name -ErrorAction SilentlyContinue){
    Stop-ScheduledTask -TaskName $a.Name -ErrorAction SilentlyContinue } }

  'StartProcess' = { param($a,$c)
   Start-Process -FilePath $a.ExecutablePath -ArgumentList $a.Argument -WorkingDirectory $a.WorkingDirectory }

  'GetProcesses' = { param($a,$c)
   @(Get-CimInstance Win32_Process -Filter "Name='MoonlightClipboard.exe'" -ErrorAction SilentlyContinue |
    Where-Object {$_.ExecutablePath -eq $a.ExecutablePath} |
    ForEach-Object {[pscustomobject]@{ProcessId=[int]$_.ProcessId;ParentProcessId=[int]$_.ParentProcessId;SessionId=[int]$_.SessionId;ExecutablePath=[string]$_.ExecutablePath}}) }

  'StopProcess' = { param($a,$c) Stop-Process -Id $a.ProcessId -Force -ErrorAction SilentlyContinue }

  'GetListener' = { param($a,$c)
   @(Get-NetTCPConnection -LocalPort $a.Port -State Listen -ErrorAction SilentlyContinue |
    ForEach-Object {[pscustomobject]@{LocalAddress=[string]$_.LocalAddress;LocalPort=[int]$_.LocalPort;OwningProcess=[int]$_.OwningProcess}}) }

  # Exact previous firewall state.
  #
  # A rule's scoping is spread over six sibling CIM objects, and a snapshot
  # that reads only some of them silently WIDENS the rule when it is replayed.
  # On a real host Get-NetFirewallApplicationFilter has NO Service property -
  # verification/windows-firewall-native-fields.json records
  # application_has_service=false with service_filter_value=Any coming from
  # Get-NetFirewallServiceFilter - so reading $application.Service yields
  # nothing and a service-scoped rule comes back as an any-service rule. The
  # interface and security filters were dropped the same way.
  #
  # Two tiers of fields are therefore distinguished:
  #   * exposure-relevant (address/port/program/service/interface/security/
  #     edge-traversal/direction/action/profile/enabled): unreadable => the
  #     snapshot is refused, because replaying it could widen the rule.
  #   * cosmetic (display name, description, group): unreadable => noted in
  #     Gaps, never a reason to block an otherwise exact restore.
  # The refusal happens here, during backup, so the helper is never stopped and
  # the rule is never touched when its exact state is unknown.
  'GetFirewall' = { param($a,$c)
   $rule=Get-NetFirewallRule -Name $a.Name -ErrorAction SilentlyContinue
   if(-not $rule){return $null}
   $missing=[System.Collections.Generic.List[string]]::new()
   $notes=[System.Collections.Generic.List[string]]::new()
   $parameters=[ordered]@{}
   $readOne={
    param([string]$label,$object,$property,[bool]$required)
    if($null -eq $object){
     if($required){$missing.Add($label + ' (filter object absent)')}else{$notes.Add($label + ' (filter object absent)')}
     return $null
    }
    $member=$object.PSObject.Properties[$property]
    if(-not $member){
     if($required){$missing.Add($label)}else{$notes.Add($label + ' (property absent)')}
     return $null
    }
    return $member.Value
   }
   # Filter objects are read once each. Both failure shapes are fatal: a filter
   # cmdlet that is absent means the module is not fully usable, and a cmdlet
   # that returns nothing means that dimension is unknown. Either way the exact
   # previous scope cannot be proven, so the snapshot is refused rather than
   # replayed with a dimension missing (which is how a restricted rule widens).
   $filterNames=@{
    address='Get-NetFirewallAddressFilter'
    port='Get-NetFirewallPortFilter'
    application='Get-NetFirewallApplicationFilter'
    service='Get-NetFirewallServiceFilter'
    interface='Get-NetFirewallInterfaceFilter'
    interfaceType='Get-NetFirewallInterfaceTypeFilter'
    security='Get-NetFirewallSecurityFilter'
   }
   $filters=@{}
   foreach($key in $filterNames.Keys){
    $cmdlet=$filterNames[$key]
    if(-not (Get-Command $cmdlet -ErrorAction SilentlyContinue)){
     $missing.Add($cmdlet + ' is not available on this host')
     $filters[$key]=$null
     continue
    }
    $filters[$key]=$rule | & $cmdlet -ErrorAction SilentlyContinue
   }
   $address=$filters['address']
   $port=$filters['port']
   $application=$filters['application']
   $service=$filters['service']
   $interface=$filters['interface']
   $interfaceType=$filters['interfaceType']
   $security=$filters['security']

   $parameters['Name']=[string]$rule.Name
   # Cosmetic attributes: preserved when readable, recorded as a gap when not.
   foreach($pair in @(@('DisplayName','DisplayName'),@('Description','Description'),@('Group','Group'))){
    $value=& $readOne $pair[0] $rule $pair[1] $false
    if($null -ne $value -and [string]$value -ne ''){$parameters[$pair[0]]=[string]$value}
   }
   # A rule that lives in a GPO store cannot be recreated locally: doing so
   # would leave a weaker local rule behind after a rollback.
   $policyStore=& $readOne 'PolicyStore' $rule 'PolicyStore' $false
   if($null -ne $policyStore -and [string]$policyStore -ne '' -and [string]$policyStore -ne 'PersistentStore'){
    $missing.Add('PolicyStore=' + [string]$policyStore + ' (rule is not locally managed)')
   }

   # Exposure-relevant scalar fields.
   foreach($pair in @(
    @('Enabled',$rule,'Enabled'),
    @('Direction',$rule,'Direction'),
    @('Action',$rule,'Action'),
    @('Profile',$rule,'Profile'),
    @('Protocol',$port,'Protocol'),
    @('Program',$application,'Program'),
    @('Service',$service,'Service'),
    @('InterfaceType',$interfaceType,'InterfaceType'),
    @('Authentication',$security,'Authentication'),
    @('Encryption',$security,'Encryption'),
    @('OverrideBlockRules',$security,'OverrideBlockRules'),
    @('LocalUser',$security,'LocalUser'),
    @('RemoteUser',$security,'RemoteUser'),
    @('RemoteMachine',$security,'RemoteMachine'))){
    $value=& $readOne $pair[0] $pair[1] $pair[2] $true
    if($null -eq $value){continue}
    if($value -is [bool]){$parameters[$pair[0]]=$value}
    elseif([string]$value -ne ''){$parameters[$pair[0]]=[string]$value}
   }
   # Exposure-relevant list fields. 'Any' is a real value and is kept; empty
   # entries are dropped so the replay cannot pass an empty scope to
   # New-NetFirewallRule (which would be rejected, or worse, read as any).
   foreach($pair in @(
    @('LocalAddress',$address,'LocalAddress'),
    @('RemoteAddress',$address,'RemoteAddress'),
    @('LocalPort',$port,'LocalPort'),
    @('RemotePort',$port,'RemotePort'),
    @('InterfaceAlias',$interface,'InterfaceAlias'))){
    $value=& $readOne $pair[0] $pair[1] $pair[2] $true
    if($null -eq $value){continue}
    $list=@($value | Where-Object {$null -ne $_ -and [string]$_ -ne ''})
    if($list.Count -gt 0){$parameters[$pair[0]]=[string[]]$list}
   }
   # IcmpType carries information only for ICMP rules and cannot be replayed
   # alongside TCP/UDP, so it is kept only when it is non-empty.
   $icmp=& $readOne 'IcmpType' $port 'IcmpType' $false
   if($null -ne $icmp){
    $list=@($icmp | Where-Object {$null -ne $_ -and [string]$_ -ne ''})
    if($list.Count -gt 0){$parameters['IcmpType']=[string[]]$list}
   }
   # EdgeTraversalPolicy is exposed on the rule on some builds and on the
   # security filter on others; a missing value would silently reopen (or
   # close) an edge-traversal gap, so it is required.
   $edge=$null
   foreach($probe in @($rule,$security)){
    if($null -eq $probe){continue}
    $property=$probe.PSObject.Properties['EdgeTraversalPolicy']
    if($property){$edge=$property.Value;break}
   }
   if($null -ne $edge -and [string]$edge -ne ''){$parameters['EdgeTraversalPolicy']=[string]$edge}
   else{$missing.Add('EdgeTraversalPolicy')}

   # Fail closed at capture time, before the helper is stopped and before the
   # rule is removed. A partial snapshot is never returned as if it were exact.
   if($missing.Count -gt 0){
    throw ('The existing firewall rule ' + $rule.Name + ' could not be snapshotted exactly (unreadable: ' + ($missing -join ', ') + ')')
   }
   [pscustomobject]@{
    Name=[string]$rule.Name
    Parameters=$parameters
    HasEdgeTraversal=([string]$edge -ne '' -and $null -ne $edge)
    Complete=$true
    Missing=@()
    Notes=@($notes) } }

  'NewFirewall' = { param($a,$c)
   $parameters=@{}
   foreach($key in $a.Parameters.Keys){
    $value=$a.Parameters[$key]
    if($value -is [string] -and $value.Length -eq 0){continue}
    if($null -eq $value){continue}
    $parameters[$key]=$value
   }
   New-NetFirewallRule @parameters -ErrorAction Stop | Out-Null }

  'RemoveFirewall' = { param($a,$c)
   if(Get-NetFirewallRule -Name $a.Name -ErrorAction SilentlyContinue){
    Remove-NetFirewallRule -Name $a.Name -ErrorAction Stop } }

  'Sleep' = { param($a,$c) Start-Sleep -Milliseconds $a.Milliseconds }
 }
}

function Invoke-Op {
 param([hashtable]$Ops,[string]$Name,[hashtable]$Arguments=@{},[hashtable]$Context=@{})
 if(-not $Ops.ContainsKey($Name)){throw ('Installer operation is not implemented: ' + $Name)}
 & $Ops[$Name] $Arguments $Context
}

# Restrictive rule created by this installer. Kept in one place so activation
# and the test simulation describe the same object.
function New-FirewallParameters {
 param([string]$Name,[string]$Bind,[int]$Port,[string]$WebServer,[string]$ExecutablePath)
 return [ordered]@{
  Name=$Name
  DisplayName=$Name
  Enabled='True'
  Direction='Inbound'
  Action='Allow'
  Profile='Any'
  Protocol='TCP'
  LocalAddress=$Bind
  LocalPort=$Port
  RemoteAddress=$WebServer
  Program=$ExecutablePath
  EdgeTraversalPolicy='Block'
 }
}

# ---------------------------------------------------------------------------
# Health check: the helper must be a supervised child running from the exact
# installed path in an interactive session and owning the single listener.
# ---------------------------------------------------------------------------
function Get-HelperState {
 param([hashtable]$Ops,[hashtable]$Context,[string]$ExecutablePath,[int]$Port,[string]$Bind)
 $processes=@(Invoke-Op $Ops 'GetProcesses' @{ExecutablePath=$ExecutablePath} $Context)
 $listener=@(Invoke-Op $Ops 'GetListener' @{Port=$Port} $Context)
 $problems=@()
 if($processes.Count -lt 1){$problems+='no MoonlightClipboard.exe process is running'}
 $children=@($processes | Where-Object {$_.ParentProcessId -and ($processes.ProcessId -contains $_.ParentProcessId)})
 if($processes.Count -gt 0 -and $children.Count -eq 0){$problems+='no supervised child process was found'}
 foreach($process in $processes){
  if($process.SessionId -eq 0){$problems+=('process ' + $process.ProcessId + ' is in session 0')}
  if($process.ExecutablePath -ne $ExecutablePath){$problems+=('process ' + $process.ProcessId + ' runs from an unexpected path')}
 }
 if($listener.Count -ne 1){$problems+=('expected exactly one listener on ' + $Port + ', found ' + $listener.Count)}
 else{
  if($listener[0].LocalAddress -ne $Bind){$problems+=('listener address ' + $listener[0].LocalAddress + ' differs from ' + $Bind)}
  if(-not ($processes.ProcessId -contains $listener[0].OwningProcess)){$problems+='listener is not owned by a bridge process we started'}
 }
 [pscustomobject]@{Problems=$problems;Processes=$processes;Listener=$listener}
}

function Wait-HelperHealthy {
 param([hashtable]$Ops,[hashtable]$Context,[string]$ExecutablePath,[int]$Port,[string]$Bind,[int]$Attempts=10,[int]$DelayMilliseconds=300)
 $state=$null
 for($attempt=0;$attempt -lt $Attempts;$attempt++){
  $state=Get-HelperState -Ops $Ops -Context $Context -ExecutablePath $ExecutablePath -Port $Port -Bind $Bind
  if($state.Problems.Count -eq 0){return $state}
  Invoke-Op $Ops 'Sleep' @{Milliseconds=$DelayMilliseconds} $Context
 }
 return $state
}

# ---------------------------------------------------------------------------
# The installer. Dot-source this file to get the functions without installing;
# executing it performs the install. See tests/install.Tests.ps1.
# ---------------------------------------------------------------------------
function Invoke-MoonlightInstall {
 param(
  [string]$InstallDir='C:\ProgramData\MoonlightClipboard',
  [string]$DesktopUser,
  [string]$Bind,
  [string]$WebServer,
  [int]$Port=47991,
  [string]$Compiler='C:\Windows\Microsoft.NET\Framework64\v4.0.30319\csc.exe',
  [switch]$KeepStaging,
  [hashtable]$Operations=$null,
  [string]$BuildRoot=$null,
  [string]$TaskName='MoonlightClipboard',
  [string]$FirewallName='MoonlightClipboard-WebServer-Only'
 )
 if([string]::IsNullOrEmpty($BuildRoot)){$BuildRoot=Split-Path -Parent $ScriptPath}
 if([string]::IsNullOrWhiteSpace($DesktopUser) -or [string]::IsNullOrWhiteSpace($Bind) -or [string]::IsNullOrWhiteSpace($WebServer)){
  throw 'Specify -DesktopUser, -Bind, and -WebServer.'
 }
 $ops=if($Operations){$Operations}else{New-DefaultOperations}
 $context=@{InstallDir=$InstallDir;BuildRoot=$BuildRoot;TaskName=$TaskName;FirewallName=$FirewallName;DesktopUser=$DesktopUser}
 $executable=Join-Path $InstallDir 'MoonlightClipboard.exe'
 $configPath=Join-Path $InstallDir 'config.json'
 $stamp=Get-Date -Format 'yyyyMMdd-HHmmss'
 $staging=Join-Path $InstallDir ('.staging-' + $stamp)
 $rollback=Join-Path $InstallDir ('.rollback-' + $stamp)
 $stage='preflight'
 # Ordered history of the steps this run actually entered, so a failure report
 # shows the sequence rather than only the last stage name.
 $events=[System.Collections.Generic.List[string]]::new()
 $report=[ordered]@{
  Outcome='unknown';ExitCode=1;Stage=$stage;Message=''
  Installed=$InstallDir;Task=$TaskName;Firewall=$FirewallName
  Staging=$staging;Rollback=$rollback
  Previous=@{InstallDirExisted=$false;Task=$false;TaskWasRunning=$false;HelperWasRunning=$false;Firewall=$false;Config=$false;Artifacts=@();Acl=$null}
  Activation=@{Files=$false;Config=$false;Acl=$false;Firewall=$false;Task=$false;Started=$false;Healthy=$false}
  Problems=@();RollbackSteps=@();RollbackProblems=@();Gaps=@()
  Processes=@();Listener=@();Events=$events
 }
 function Add-Event([string]$name){$events.Add($name)}

 # -- step 1: preflight. Nothing on the machine is touched yet. -------------
 try {
  $stage='preflight'
  Add-Event 'preflight'
  if($Port -lt 1024 -or $Port -gt 65535){throw "Port $Port is outside 1024-65535"}
  if(-not (Invoke-Op $ops 'CompilerExists' @{Path=$Compiler} $context)){throw "C# compiler not found at $Compiler"}
  $sid=Invoke-Op $ops 'ResolveSid' @{User=$DesktopUser} $context
  if([string]::IsNullOrEmpty($sid)){throw "Could not resolve $DesktopUser to a SID"}

  # Config must either be absent (first install, generated below) or already
  # match the requested endpoint. A mismatch aborts before the service stops.
  $existingConfig=$null
  if(Invoke-Op $ops 'FileExists' @{Path=$configPath} $context){
   $existingConfig=Invoke-Op $ops 'ReadFile' @{Path=$configPath} $context | ConvertFrom-Json
   if($existingConfig.bind -ne $Bind -or $existingConfig.port -ne $Port -or @($existingConfig.allowed_ips).Count -ne 1 -or $existingConfig.allowed_ips[0] -ne $WebServer){throw 'Existing endpoint differs: explicit configuration migration required'}
   if([string]::IsNullOrEmpty($existingConfig.token) -or $existingConfig.token -notmatch $TokenPattern){throw 'Existing config.json has no valid token; refusing to replace it silently'}
  }

  # The pre-mutation state of both directories is captured HERE, before any
  # write at all. Reading it again during backup would be wrong: staging is
  # created *inside* the install directory, and creating a directory creates
  # its missing parents too, so by backup time a directory this run created
  # looks pre-existing. A rollback would then "restore" an ACL onto a
  # directory that should have been removed, and would leave a directory
  # behind that was not there before.
  $preMutationDirs=[ordered]@{}
  $preMutationAcls=@{}
  foreach($directory in @($InstallDir,$BuildRoot)){
   $existed=Invoke-Op $ops 'DirectoryExists' @{Path=$directory} $context
   $preMutationDirs[$directory]=$existed
   if($existed){
    $sddl=Invoke-Op $ops 'GetDirectoryAcl' @{Path=$directory} $context
    # An ACL captured after a mutation cannot be restored either; refuse while
    # the previous helper is still running.
    if([string]::IsNullOrEmpty($sddl)){throw ('The existing ACL of ' + $directory + ' could not be captured; refusing to change it')}
    $preMutationAcls[$directory]=$sddl
   }
  }
  # -- step 2: build into a versioned staging directory. The live helper keeps
  # running, so a build or test failure never interrupts it.
  $stage='staging'
  Add-Event 'staging'
  Invoke-Op $ops 'EnsureDirectory' @{Path=$staging} $context
  Invoke-Op $ops 'Compile' @{
   Compiler=$Compiler;Target='library';Output=(Join-Path $staging 'Core.dll')
   WorkingDirectory=$BuildRoot;ErrorMessage='Core compile failed'
   References=@('System.Web.Extensions.dll','System.Windows.Forms.dll','System.Drawing.dll')
   Sources=@('Core.cs','Snapshot.cs','SupervisorLog.cs')} $context
  Invoke-Op $ops 'Compile' @{
   Compiler=$Compiler;Target='winexe';Output=(Join-Path $staging 'MoonlightClipboard.exe')
   WorkingDirectory=$BuildRoot;ErrorMessage='Native compile failed'
   References=@((Join-Path $staging 'Core.dll'),'System.Web.Extensions.dll','System.Windows.Forms.dll')
   Sources=@('Native.cs')} $context
  Invoke-Op $ops 'Compile' @{
   Compiler=$Compiler;Target='winexe';Output=(Join-Path $staging 'ClipboardGuard.exe')
   WorkingDirectory=$BuildRoot;ErrorMessage='Guard compile failed'
   References=@((Join-Path $staging 'Core.dll'),'System.Windows.Forms.dll')
   Sources=@('Guard.cs')} $context
  foreach($name in $Artifacts){
   if(-not (Invoke-Op $ops 'FileExists' @{Path=(Join-Path $staging $name)} $context)){throw "Staging build is incomplete: $name missing"}
  }

  # Run the fake-backend regression suite against the newly built assemblies.
  # This never touches the system clipboard (Tests.cs uses a fake IClipboard and
  # fake IDataObject), so it is safe to run before the live helper is interrupted.
  Invoke-Op $ops 'Compile' @{
   Compiler=$Compiler;Target='exe';Output=(Join-Path $staging 'Tests.exe')
   WorkingDirectory=$staging;ErrorMessage='Test compile failed'
   References=@((Join-Path $staging 'Core.dll'),'System.Web.Extensions.dll','System.Windows.Forms.dll','System.Drawing.dll')
   Sources=@((Join-Path $BuildRoot 'Tests.cs'))} $context
  Invoke-Op $ops 'RunRegression' @{Executable=(Join-Path $staging 'Tests.exe');WorkingDirectory=$staging} $context
  foreach($name in $TestArtifacts){
   Invoke-Op $ops 'RemoveFile' @{Path=(Join-Path $staging $name)} $context
  }

  # -- step 3: back up the complete previous state BEFORE stopping anything.
  # Failure here aborts with the old helper still running.
  $stage='backup'
  Add-Event 'backup'
  Invoke-Op $ops 'EnsureDirectory' @{Path=$rollback} $context
  $previous=@{Task=$null;TaskWasRunning=$false;HelperWasRunning=$false;Firewall=$null;Config=$null;Acl=@{};Directories=@{};Artifacts=@();Processes=@()}
  foreach($name in $Artifacts){
   $current=Join-Path $InstallDir $name
   if(Invoke-Op $ops 'FileExists' @{Path=$current} $context){
    Invoke-Op $ops 'CopyFile' @{Source=$current;Destination=(Join-Path $rollback $name)} $context
    $previous.Artifacts+=@($name)
   }
  }
  if(Invoke-Op $ops 'FileExists' @{Path=$configPath} $context){
   Invoke-Op $ops 'CopyFile' @{Source=$configPath;Destination=(Join-Path $rollback 'config.json')} $context
   $previous.Config=Join-Path $rollback 'config.json'
  }
  foreach($directory in @($InstallDir,$BuildRoot)){
   # Use the state captured before this run wrote anything (see preflight).
   # Reading it again here would report staging's parent as pre-existing.
   $existed=[bool]$preMutationDirs[$directory]
   $previous.Directories[$directory]=$existed
   if($existed){$previous.Acl[$directory]=$preMutationAcls[$directory]}
  }
  $previous.Task=Invoke-Op $ops 'GetTask' @{Name=$TaskName} $context
  if($previous.Task){
   $previous.TaskWasRunning=($previous.Task.State -eq 'Running')
   if([string]::IsNullOrEmpty($previous.Task.Xml)){throw 'The existing scheduled task definition could not be exported; refusing to modify it'}
  }
  $previous.Firewall=Invoke-Op $ops 'GetFirewall' @{Name=$FirewallName} $context
  # GetFirewall fails closed on an incomplete snapshot, so reaching here means
  # every exposure-relevant field was readable. Defensive re-check in case an
  # injected operation table returns a partial object instead.
  if($previous.Firewall -and -not $previous.Firewall.Complete){
   throw ('The existing firewall rule ' + $FirewallName + ' could not be snapshotted exactly (missing: ' + (@($previous.Firewall.Missing) -join ', ') + '); refusing to replace a rule that could not be restored precisely')
  }
  if($previous.Firewall -and -not $previous.Firewall.HasEdgeTraversal){
   $report.Gaps+='previous firewall rule edge-traversal policy could not be read; restore omits it'
  }
  if($previous.Firewall){
   foreach($note in @($previous.Firewall.Notes)){
    if($note){$report.Gaps+=('previous firewall rule: ' + $note + ' was not preserved')}
   }
  }
  $previous.Processes=@(Invoke-Op $ops 'GetProcesses' @{ExecutablePath=$executable} $context)
  # "Was a helper running" is exactly "is a helper process alive", not "is a
  # task registered". A helper started by hand has to be restarted after a
  # rollback just like a task-started one.
  $previous.HelperWasRunning=($previous.Processes.Count -gt 0)
  if($previous.TaskWasRunning -and -not $previous.HelperWasRunning){
   $report.Gaps+='the scheduled task reported Running but no helper process was visible; only the task state is restored'
  }
  $report.Previous=@{
   InstallDirExisted=$previous.Directories[$InstallDir];BuildRootExisted=$previous.Directories[$BuildRoot];Task=[bool]$previous.Task
   TaskWasRunning=$previous.TaskWasRunning;HelperWasRunning=$previous.HelperWasRunning
   Firewall=[bool]$previous.Firewall;Config=[bool]$previous.Config;Artifacts=@($previous.Artifacts)
   Acl=$previous.Acl;Directories=$previous.Directories
  }
  $report.Activation=@{Files=$false;Config=$false;Acl=$false;Firewall=$false;Task=$false;Started=$false;Healthy=$false}
 } catch {
   # Nothing was changed on the machine: the live helper, task, firewall, files
   # and ACLs are exactly as they were. Drop the partial staging/backup dirs.
   if(Invoke-Op $ops 'DirectoryExists' @{Path=$staging} $context){Invoke-Op $ops 'RemoveOwnedDirectory' @{Path=$staging} $context}
   if(Invoke-Op $ops 'DirectoryExists' @{Path=$rollback} $context){Invoke-Op $ops 'RemoveOwnedDirectory' @{Path=$rollback} $context}
   $report.Outcome='aborted-before-change';$report.ExitCode=1;$report.Stage=$stage
   $report.Message=('Installation aborted before any change (' + $stage + '): ' + $_.Exception.Message + '. The previous helper was not stopped.')
   return [pscustomobject]@{ExitCode=1;Outcome=$report.Outcome;Stage=$stage;Message=$report.Message;Report=[pscustomobject]$report}
 }

 # -- step 4: activation transaction. --------------------------------------
 $activationFailure=$null
 $activationStage=$null
 try {
  # 4a. stop the old helper
  $stage='stop';$activationStage=$stage
  Add-Event 'stop'
  if($previous.Task){Invoke-Op $ops 'StopTask' @{Name=$TaskName} $context}
  foreach($process in @($previous.Processes)){Invoke-Op $ops 'StopProcess' @{ProcessId=$process.ProcessId} $context}
  Invoke-Op $ops 'Sleep' @{Milliseconds=300} $context
  $remaining=@(Invoke-Op $ops 'GetProcesses' @{ExecutablePath=$executable} $context)
  if($remaining.Count -gt 0){
   $ids=@($remaining | ForEach-Object {$_.ProcessId} | Sort-Object)
   throw ('the previous helper is still running: process ' + ($ids -join ','))
  }

  # 4b. switch binaries, verifying each copy against the staged build
  $stage='files';$activationStage=$stage
  Add-Event 'files'
  foreach($name in $Artifacts){
   $target=Join-Path $InstallDir $name
   Invoke-Op $ops 'CopyFile' @{Source=(Join-Path $staging $name);Destination=$target} $context
   if(-not (Invoke-Op $ops 'FileExists' @{Path=$target} $context)){throw "Switched file is missing: $name"}
   $stagedHash=Invoke-Op $ops 'GetFileHash' @{Path=(Join-Path $staging $name)} $context
   $installedHash=Invoke-Op $ops 'GetFileHash' @{Path=$target} $context
   if($stagedHash -ne $installedHash){throw "Switched file does not match the staged build: $name"}
  }
  $report.Activation.Files=$true

  # 4c. config: generated only when absent, otherwise left byte-identical
  $stage='config';$activationStage=$stage
  Add-Event 'config'
  if(-not $previous.Config){
   $bytes=New-Object byte[] 32
   $rng=[System.Security.Cryptography.RandomNumberGenerator]::Create()
   try{$rng.GetBytes($bytes)}finally{$rng.Dispose()}
   $token=([BitConverter]::ToString($bytes)).Replace('-','').ToLowerInvariant()
   Invoke-Op $ops 'WriteFile' @{Path=$configPath;Content=(@{bind=$Bind;port=$Port;allowed_ips=@($WebServer);token=$token}|ConvertTo-Json)} $context
   $written=Invoke-Op $ops 'ReadFile' @{Path=$configPath} $context | ConvertFrom-Json
   if($written.bind -ne $Bind -or $written.port -ne $Port -or @($written.allowed_ips).Count -ne 1 -or $written.allowed_ips[0] -ne $WebServer -or $written.token -notmatch $TokenPattern){throw 'Generated config.json did not read back as written'}
   $report.Activation.Config='created'
  }else{
   $unchanged=Invoke-Op $ops 'ReadFile' @{Path=$configPath} $context
   $backedUp=Invoke-Op $ops 'ReadFile' @{Path=$previous.Config} $context
   if($unchanged -ne $backedUp){throw 'config.json changed during activation although it should have been preserved'}
   $report.Activation.Config='preserved'
  }

  # 4d. ACL on the install and build directories
  $stage='acl';$activationStage=$stage
  Add-Event 'acl'
  Invoke-Op $ops 'SetDirectoryAcl' @{Path=$InstallDir;Sid=$sid} $context
  if($BuildRoot -ne $InstallDir){Invoke-Op $ops 'SetDirectoryAcl' @{Path=$BuildRoot;Sid=$sid} $context}
  $report.Activation.Acl=$true

  # 4e. firewall: replace our named rule with the exact scoped definition
  $stage='firewall';$activationStage=$stage
  Add-Event 'firewall'
  if(Invoke-Op $ops 'GetFirewall' @{Name=$FirewallName} $context){Invoke-Op $ops 'RemoveFirewall' @{Name=$FirewallName} $context}
  Invoke-Op $ops 'NewFirewall' @{Parameters=(New-FirewallParameters -Name $FirewallName -Bind $Bind -Port $Port -WebServer $WebServer -ExecutablePath $executable)} $context
  if(-not (Invoke-Op $ops 'GetFirewall' @{Name=$FirewallName} $context)){throw 'the firewall rule was not created'}
  $report.Activation.Firewall=$true

  # 4f. task definition
  $stage='task';$activationStage=$stage
  Add-Event 'task'
  $action=@{Execute=$executable;Argument='--supervise';WorkingDirectory=$InstallDir}
  $definition=@{
   Action=@(@{Execute=$executable;Argument='--supervise';WorkingDirectory=$InstallDir})
   Principal=@{UserId=$DesktopUser;LogonType='Interactive';RunLevel='Limited'}
   Trigger=@(@{AtLogOn=$true;User=$DesktopUser})
   Settings=@{
    ExecutionTimeLimit='PT0S'
    RestartCount=999
    RestartInterval='PT1M'
    MultipleInstances='IgnoreNew'
    StartWhenAvailable=$true
    AllowStartOnBatteries=$true
    DontStopOnBatteries=$true
   }
  }
  Invoke-Op $ops 'RegisterTask' @{Name=$TaskName;Definition=$definition} $context
  $registered=Invoke-Op $ops 'GetTask' @{Name=$TaskName} $context
  if(-not $registered){throw 'the scheduled task was not registered'}
  $report.Activation.Task=$true

  # 4g. start + health verification
  $stage='start';$activationStage=$stage
  Add-Event 'start'
  Invoke-Op $ops 'StartTask' @{Name=$TaskName} $context
  $report.Activation.Started=$true

  $stage='postcheck';$activationStage=$stage
  Add-Event 'postcheck'
  $health=Wait-HelperHealthy -Ops $ops -Context $context -ExecutablePath $executable -Port $Port -Bind $Bind
  $report.Processes=@($health.Processes);$report.Listener=@($health.Listener)
  if($health.Problems.Count -gt 0){
   $report.Problems=@($health.Problems)
   throw ('Install verification failed: ' + ($health.Problems -join '; '))
  }
  $report.Activation.Healthy=$true
 } catch {
  $activationFailure=$_.Exception.Message
 }

 if($null -eq $activationFailure){
  # -- success. Staging is disposable; the rollback snapshot is kept as the
  # documented recovery point for this version pair.
  if(-not $KeepStaging){Invoke-Op $ops 'RemoveOwnedDirectory' @{Path=$staging} $context}
  $report.Outcome='installed';$report.ExitCode=0;$report.Stage='complete'
  $report.Message='Installation activated and verified.'
  return [pscustomobject]@{ExitCode=0;Outcome=$report.Outcome;Stage=$report.Stage;Message=$report.Message;Report=[pscustomobject]$report}
 }

 # -- step 5: rollback. Terminate this run's helper, restore every layer that
 # could have been changed, then prove the previous helper is healthy again.
 # Each layer is attempted even if an earlier one fails, and every failure is
 # reported instead of being masked by the first one.
 $rollbackProblems=@()
 function Add-RollbackStep([string]$name,[bool]$ok,[string]$detail){
  $report.RollbackSteps+=@{Step=$name;Ok=$ok;Detail=$detail}
 }

 $report.Stage='rollback'
 Add-Event ('rollback-from:' + $activationStage)
 # Stop only the helper this activation started. A pre-existing process (one
 # that survived a failed stop) is the thing being preserved, not a leftover.
 $previousIds=@($previous.Processes | ForEach-Object {$_.ProcessId})
 $newHelpers=@(Invoke-Op $ops 'GetProcesses' @{ExecutablePath=$executable} $context | Where-Object {$previousIds -notcontains $_.ProcessId})
 foreach($process in $newHelpers){
  try{Invoke-Op $ops 'StopProcess' @{ProcessId=$process.ProcessId} $context;Add-RollbackStep 'stop-new-helper' $true ('process ' + $process.ProcessId)}catch{Add-RollbackStep 'stop-new-helper' $false $_.Exception.Message}
 }
 try{Invoke-Op $ops 'StopTask' @{Name=$TaskName} $context;Add-RollbackStep 'stop-new-task' $true ''}catch{Add-RollbackStep 'stop-new-task' $false $_.Exception.Message}

 foreach($name in $Artifacts){
  try{
   $saved=Join-Path $rollback $name
   $target=Join-Path $InstallDir $name
   if(Invoke-Op $ops 'FileExists' @{Path=$saved} $context){
    Invoke-Op $ops 'CopyFile' @{Source=$saved;Destination=$target} $context
    $savedHash=Invoke-Op $ops 'GetFileHash' @{Path=$saved} $context
    $targetHash=Invoke-Op $ops 'GetFileHash' @{Path=$target} $context
    if($savedHash -ne $targetHash){throw "restored file does not match the backup: $name"}
    Add-RollbackStep ('restore-file:' + $name) $true ''
   }else{
    if(Invoke-Op $ops 'FileExists' @{Path=$target} $context){Invoke-Op $ops 'RemoveFile' @{Path=$target} $context}
    Add-RollbackStep ('remove-new-file:' + $name) $true 'no previous file existed'
   }
  }catch{Add-RollbackStep ('restore-file:' + $name) $false $_.Exception.Message}
 }

 try{
  if($previous.Config){
   Invoke-Op $ops 'CopyFile' @{Source=$previous.Config;Destination=$configPath} $context
   $restored=Invoke-Op $ops 'ReadFile' @{Path=$configPath} $context
   $expected=Invoke-Op $ops 'ReadFile' @{Path=$previous.Config} $context
   if($restored -ne $expected){throw 'restored config.json differs from the backup'}
   Add-RollbackStep 'restore-config' $true ''
  }else{
   if(Invoke-Op $ops 'FileExists' @{Path=$configPath} $context){Invoke-Op $ops 'RemoveFile' @{Path=$configPath} $context}
   if(Invoke-Op $ops 'FileExists' @{Path=$configPath} $context){throw 'config.json created by this run is still present'}
   Add-RollbackStep 'remove-new-config' $true 'no previous config existed'
  }
 }catch{Add-RollbackStep 'restore-config' $false $_.Exception.Message}

 # A first install has nothing to restore, so its staging and rollback trees
 # are pure leftovers *inside* the directory this run created. They are this
 # run's own uniquely named directories, so they go before the install
 # directory is examined - otherwise it can never be empty.
 $installDirWasCreated=(-not [bool]$previous.Directories[$InstallDir])
 if($installDirWasCreated){
  foreach($owned in @($staging,$rollback)){
   try{
    if(Invoke-Op $ops 'DirectoryExists' @{Path=$owned} $context){Invoke-Op $ops 'RemoveOwnedDirectory' @{Path=$owned} $context}
   }catch{$report.Gaps+=('this run''s directory could not be removed: ' + $owned + ': ' + $_.Exception.Message)}
  }
 }

 foreach($directory in $previous.Directories.Keys){
  $existedBefore=[bool]$previous.Directories[$directory]
  if($existedBefore){
   try{
    if([string]::IsNullOrEmpty($previous.Acl[$directory])){throw 'no previous ACL was captured'}
    Invoke-Op $ops 'RestoreDirectoryAcl' @{Path=$directory;Sddl=$previous.Acl[$directory]} $context
    $restoredAcl=Invoke-Op $ops 'GetDirectoryAcl' @{Path=$directory} $context
    if($restoredAcl -ne $previous.Acl[$directory]){throw 'the ACL still differs from the captured one after restore'}
    Add-RollbackStep ('restore-acl:' + $directory) $true ''
   }catch{Add-RollbackStep ('restore-acl:' + $directory) $false $_.Exception.Message}
  }
  else{
   # This run created the directory; removing it returns the machine to its
   # prior shape - but only if it is empty. Unknown content (a file written
   # concurrently by someone else, an operator's log) is never force-deleted;
   # the directory is kept and the leftover is reported.
   try{
    if($directory -eq $BuildRoot){Add-RollbackStep ('remove-created-directory:' + $directory) $true 'source directory is retained'}
    elseif(Invoke-Op $ops 'DirectoryExists' @{Path=$directory} $context){
     $gone=Invoke-Op $ops 'RemoveDirectoryIfEmpty' @{Path=$directory} $context
     if($gone){
      if(Invoke-Op $ops 'DirectoryExists' @{Path=$directory} $context){throw 'the directory is still present after removal'}
      Add-RollbackStep ('remove-created-directory:' + $directory) $true 'the directory did not exist before this run'
     }else{
      Add-RollbackStep ('remove-created-directory:' + $directory) $false 'the directory this run created holds content it does not own and was left in place'
     }
    }else{Add-RollbackStep ('remove-created-directory:' + $directory) $true 'nothing to remove'}
   }catch{Add-RollbackStep ('remove-created-directory:' + $directory) $false $_.Exception.Message}
  }
 }

 try{
  Invoke-Op $ops 'RemoveFirewall' @{Name=$FirewallName} $context
  if($previous.Firewall){
   Invoke-Op $ops 'NewFirewall' @{Parameters=$previous.Firewall.Parameters} $context
   $restoredFirewall=Invoke-Op $ops 'GetFirewall' @{Name=$FirewallName} $context
   if(-not $restoredFirewall){throw 'the previous firewall rule is absent after restore'}
   Add-RollbackStep 'restore-firewall' $true 'previous rule recreated from its captured filters'
  }else{
   if(Invoke-Op $ops 'GetFirewall' @{Name=$FirewallName} $context){throw 'this run''s firewall rule is still present'}
   Add-RollbackStep 'remove-new-firewall' $true 'no previous rule existed'
  }
 }catch{Add-RollbackStep 'restore-firewall' $false $_.Exception.Message}

 try{
  if($previous.Task){
   if([string]::IsNullOrEmpty($previous.Task.Xml)){throw 'the previous task definition was not captured'}
   Invoke-Op $ops 'RegisterTask' @{Name=$TaskName;Xml=$previous.Task.Xml} $context
   $restoredTask=Invoke-Op $ops 'GetTask' @{Name=$TaskName} $context
   if(-not $restoredTask){throw 'the previous scheduled task is absent after restore'}
   Add-RollbackStep 'restore-task' $true 'previous definition re-registered'
  }else{
   Invoke-Op $ops 'UnregisterTask' @{Name=$TaskName} $context
   if(Invoke-Op $ops 'GetTask' @{Name=$TaskName} $context){throw 'this run''s scheduled task is still registered'}
   Add-RollbackStep 'remove-new-task' $true 'no previous task existed'
  }
 }catch{Add-RollbackStep 'restore-task' $false $_.Exception.Message}

 if($previous.HelperWasRunning){
  try{
   # A helper that survived the stop attempt is still running; starting a
   # second supervisor would just fight it for the port.
   $stillRunning=@(Invoke-Op $ops 'GetProcesses' @{ExecutablePath=$executable} $context)
   if($stillRunning.Count -gt 0){
    Add-RollbackStep 'start-previous-helper' $true ('already running as process ' + ((@($stillRunning | ForEach-Object {$_.ProcessId}) | Sort-Object) -join ','))
   }elseif($previous.Task){
    Invoke-Op $ops 'StartTask' @{Name=$TaskName} $context
    Add-RollbackStep 'start-previous-helper' $true ''
   }else{
    Invoke-Op $ops 'StartProcess' @{ExecutablePath=$executable;Argument='--supervise';WorkingDirectory=$InstallDir} $context
    Add-RollbackStep 'start-previous-helper' $true ''
   }
  }catch{Add-RollbackStep 'start-previous-helper' $false $_.Exception.Message}
  $previousHealth=Wait-HelperHealthy -Ops $ops -Context $context -ExecutablePath $executable -Port $Port -Bind $Bind
  if($previousHealth.Problems.Count -eq 0){
   Add-RollbackStep 'verify-previous-helper' $true 'previous helper is running from the installed path with the expected listener'
  }else{
   Add-RollbackStep 'verify-previous-helper' $false ($previousHealth.Problems -join '; ')
  }
  $report.Processes=@($previousHealth.Processes);$report.Listener=@($previousHealth.Listener)
 }else{
  $leftover=@(Invoke-Op $ops 'GetProcesses' @{ExecutablePath=$executable} $context)
  if($leftover.Count -eq 0){Add-RollbackStep 'verify-no-helper' $true 'no helper was running before this install'}else{Add-RollbackStep 'verify-no-helper' $false 'a helper from this run is still running'}
 }

 if(-not $KeepStaging){try{Invoke-Op $ops 'RemoveOwnedDirectory' @{Path=$staging} $context}catch{$report.Gaps+=('staging directory could not be removed: ' + $_.Exception.Message)}}

 $failedSteps=@($report.RollbackSteps | Where-Object {-not $_.Ok})
 $report.RollbackProblems=@($failedSteps | ForEach-Object {($_.Step + ': ' + $_.Detail)})
 # A rollback only counts as clean when every layer was restored *and* the
 # previous helper was either proven healthy or correctly absent.
 $healthProven=[bool](@($report.RollbackSteps | Where-Object {$_.Ok -and ($_.Step -eq 'verify-previous-helper' -or $_.Step -eq 'verify-no-helper')}).Count)
 if($failedSteps.Count -eq 0 -and $healthProven){
  $report.Outcome='rolled-back';$report.ExitCode=2
 }else{
  $report.Outcome='rollback-incomplete';$report.ExitCode=3
  if(-not $healthProven){$report.RollbackProblems+=@('the previous helper health could not be proven')}
 }
 $report.Message=('Activation failed at ' + $activationStage + ' (' + $activationFailure + '). ' + $report.Outcome + ': previous state restored from ' + $rollback + '. ' + ($report.RollbackProblems -join '; '))
 return [pscustomobject]@{ExitCode=$report.ExitCode;Outcome=$report.Outcome;Stage=$activationStage;Message=$report.Message;Report=[pscustomobject]$report}
}

if($MyInvocation.InvocationName -ne '.'){
 $result=Invoke-MoonlightInstall @PSBoundParameters
 $result.Report | ConvertTo-Json -Depth 8
 exit $result.ExitCode
}
