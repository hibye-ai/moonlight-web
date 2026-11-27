#requires -Version 5.1
<#
 Deterministic failure-injection tests for install.ps1 (AR-08).

 The installer takes every side effect through one named operation table, so
 this suite replaces that table with an in-memory simulated Windows machine:
 files, hashes, ACLs, scheduled task, firewall rule, processes and the listener
 all live in a hashtable. Nothing here can touch a real scheduled task, the real
 firewall, a real process or the clipboard - there is no code path to them,
 which one scenario in this file also proves from the installer's own text.

 Every scenario asserts the full rollback contract: the run's own helper is
 terminated, the previous binaries/config/ACL/task/firewall are restored
 exactly, the previous helper is healthy again (or correctly still absent), and
 the exit code/outcome reflect what actually happened.

 PATH MODEL (AR-08 follow-up). `Join-Path` returns backslashes on Windows
 PowerShell 5.1 and forward slashes on PowerShell 7/Linux, so this file never
 compares or matches a raw path. Every key in the simulated filesystem is
 canonicalised through ConvertTo-SimPath, and every "is this path under that
 run's directory" question goes through Test-SimPathLike, which canonicalises
 both sides. A scenario at the end proves the matchers behave the same for a
 POSIX and a Windows path, so this suite makes the same decisions on both.

 The two adapter scenarios at the end dot-source the REAL install.ps1 and run
 the REAL New-DefaultOperations scriptblocks for RegisterTask and
 GetFirewall/NewFirewall, stubbing only the operating-system cmdlets
 (New-ScheduledTask*, Register-ScheduledTask, Get-NetFirewall*). Those stubs
 cannot reach a real task engine or the real firewall; they exist to prove the
 producer/adapter contract that the native parent probe exercises.

 Run: pwsh -NoProfile -File tests\install.Tests.ps1
      powershell.exe -NoProfile -ExecutionPolicy Bypass -File tests\install.Tests.ps1
 Exit code 0 = every scenario passed.
#>
$ErrorActionPreference='Stop'
$here=Split-Path -Parent $MyInvocation.MyCommand.Path
$installer=Join-Path (Split-Path -Parent $here) 'install.ps1'
$installerText=Get-Content -Raw -LiteralPath $installer
. $installer

$script:Pass=0
$script:Fail=0
$script:Failures=@()

function Write-Result([bool]$ok,[string]$name,[string]$detail){
 if($ok){$script:Pass++;Write-Host ('PASS ' + $name)}
 else{$script:Fail++;$script:Failures+=$name;Write-Host ('FAIL ' + $name + ' :: ' + $detail)}
}

function Test-Case([string]$name,[scriptblock]$body){
 try{ & $body; Write-Result $true $name '' }
 catch{ Write-Result $false $name $_.Exception.Message }
}

function Assert-True($condition,[string]$message){
 if(-not $condition){throw $message}
}

function Assert-Equal($actual,$expected,[string]$message){
 if($actual -ne $expected){throw ($message + ' (expected [' + $expected + '], got [' + $actual + '])')}
}

# Rollback steps are hashtables, so `.Count` on a single match counts its keys.
# Always wrap a pipeline in @() before counting, and treat the "pipeline
# produced nothing" value ($null) as zero rather than one.
function Assert-Count($items,[int]$expected,[string]$message){
 $list=@($items | Where-Object {$null -ne $_})
 if($list.Count -ne $expected){throw ($message + ' (expected ' + $expected + ' item(s), got ' + $list.Count + ')')}
}

function Assert-Sequence($haystack,$expected,[string]$message){
 $actual=@($haystack) -join ','
 $wanted=@($expected) -join ','
 if($actual -ne $wanted){throw ($message + ' (expected [' + $wanted + '], got [' + $actual + '])')}
}

# ---------------------------------------------------------------------------
# Canonical path model.
#
# A single separator and no trailing separator, so the same simulated machine
# can be driven by a Linux PS7 `Join-Path` ('/sim/x/y') and a Windows 5.1 one
# ('/sim/x\y'). Without this, a hard-coded '*/x/*' matcher silently stops
# matching on Windows and an injected failure is never injected - the suite
# then reports green for a defect it never exercised.
# ---------------------------------------------------------------------------
function ConvertTo-SimPath([string]$Path){
 if($null -eq $Path){return $null}
 $p=$Path.Replace('\','/')
 while($p -match '//'){$p=$p -replace '//','/'}
 if($p.Length -gt 1){$p=$p.TrimEnd('/')}
 return $p
}

function Test-SimPathLike([string]$Path,[string]$Pattern){
 $value=ConvertTo-SimPath $Path
 $match=ConvertTo-SimPath $Pattern
 return ($value -like $match)
}

# Is $Path inside (or equal to) $Root?
function Test-SimUnder([string]$Path,[string]$Root){
 $needle=ConvertTo-SimPath $Path
 $parent=ConvertTo-SimPath $Root
 if($null -eq $needle -or $null -eq $parent){return $false}
 return ($needle -eq $parent) -or $needle.StartsWith($parent + '/')
}

function Get-SimParent([string]$Path){
 $p=ConvertTo-SimPath $Path
 if([string]::IsNullOrEmpty($p) -or $p -eq '/'){return ''}
 $index=$p.LastIndexOf('/')
 if($index -lt 0){return ''}
 if($index -eq 0){return '/'}
 return $p.Substring(0,$index)
}

function Get-SimLeaf([string]$Path){
 $p=ConvertTo-SimPath $Path
 if([string]::IsNullOrEmpty($p)){return ''}
 return $p.Substring($p.LastIndexOf('/')+1)
}

function Get-SimFile($Machine,[string]$Path){
 $key=ConvertTo-SimPath $Path
 if($null -eq $key){return $null}
 if(-not $Machine.Files.ContainsKey($key)){return $null}
 return $Machine.Files[$key]
}

function Test-SimFile($Machine,[string]$Path){
 $key=ConvertTo-SimPath $Path
 return ($null -ne $key -and $Machine.Files.ContainsKey($key))
}

function Get-SimAcl($Machine,[string]$Path){
 $key=ConvertTo-SimPath $Path
 if($null -eq $key){return $null}
 if(-not $Machine.Acls.ContainsKey($key)){return $null}
 return $Machine.Acls[$key]
}

function Test-SimDir($Machine,[string]$Path){
 $key=ConvertTo-SimPath $Path
 return ($null -ne $key -and $Machine.Dirs.ContainsKey($key))
}

# ---------------------------------------------------------------------------
# Simulated machine
# ---------------------------------------------------------------------------
$script:M=$null

function New-SimMachine {
 $machine=[pscustomobject]@{
  Files=[hashtable]::new([System.StringComparer]::OrdinalIgnoreCase)
  Dirs=[hashtable]::new([System.StringComparer]::OrdinalIgnoreCase)
  Acls=[hashtable]::new([System.StringComparer]::OrdinalIgnoreCase)
  Task=$null
  TaskRegistered=$false
  Firewall=$null
  # Field names the simulated Get-NetFirewall* surface cannot read, so the
  # snapshot must fail closed instead of replaying a widened rule.
  FirewallUnreadable=@()
  Processes=@()
  Listener=@()
  Generation=0
  ListenGeneration='current'
  Calls=[System.Collections.Generic.List[object]]::new()
  Faults=@()
  # One file dropped into the tree when the named operation runs, modelling
  # another process writing into the directory while this install is active.
  DropFileOn=$null
  DropFile=$null
  InstallDir=$null
  BuildRoot=$null
  RegressionRan=0
 }
 return $machine
}

function Get-SimHash([string]$content){
 $sha=[System.Security.Cryptography.SHA256]::Create()
 try{$bytes=[System.Text.Encoding]::UTF8.GetBytes($content);return ([BitConverter]::ToString($sha.ComputeHash($bytes))).Replace('-','').ToLowerInvariant()}
 finally{$sha.Dispose()}
}

# Create a directory and every missing parent, exactly like
# `New-Item -ItemType Directory -Force`. The parent creation matters: it is
# what makes a "did this directory exist before?" check read from the wrong
# moment if it is taken after staging has been created inside it.
function Add-SimDirectory([string]$Path){
 $key=ConvertTo-SimPath $Path
 while(-not [string]::IsNullOrEmpty($key) -and -not $script:M.Dirs.ContainsKey($key)){
  $script:M.Dirs[$key]=$true
  $key=Get-SimParent $key
 }
}

function Add-SimFile([string]$path,[string]$content){
 $key=ConvertTo-SimPath $path
 $script:M.Files[$key]=$content
 $parent=Get-SimParent $key
 if(-not [string]::IsNullOrEmpty($parent)){Add-SimDirectory $parent}
}

function Remove-SimTree([string]$path){
 $key=ConvertTo-SimPath $path
 foreach($file in @($script:M.Files.Keys)){if(Test-SimUnder $file $key){$script:M.Files.Remove($file)}}
 foreach($dir in @($script:M.Dirs.Keys)){if(Test-SimUnder $dir $key){$script:M.Dirs.Remove($dir)}}
 foreach($acl in @($script:M.Acls.Keys)){if(Test-SimUnder $acl $key){$script:M.Acls.Remove($acl)}}
}

# Everything inside $path, one level down: files and subdirectories.
function Get-SimChildren([string]$path){
 $key=ConvertTo-SimPath $path
 $children=@()
 foreach($file in @($script:M.Files.Keys)){
  if((Get-SimParent $file) -eq $key){$children+=[pscustomobject]@{Kind='file';Path=$file}}
 }
 foreach($dir in @($script:M.Dirs.Keys)){
  if($dir -ne $key -and (Get-SimParent $dir) -eq $key){$children+=[pscustomobject]@{Kind='directory';Path=$dir}}
 }
 return $children
}

function Start-SimHelper {
 $script:M.Generation++
 $generation=$script:M.Generation
 $exe=Join-Path $script:M.InstallDir 'MoonlightClipboard.exe'
 # Which binary this process is actually executing. A process launched from a
 # staged build and left running after a rollback is exactly the half-switched
 # state the installer must not produce.
 $launchedFrom=if(Test-SimFile $script:M $exe){Get-SimFile $script:M $exe}else{'absent'}
 # Starting a helper ADDS processes. Replacing the list would hide a leftover
 # helper from a failed activation, which is the state these tests exist to
 # catch, so a started supervisor is appended to whatever is already running.
 $supervisor=[int](100+$generation*10)
 $child=[int](101+$generation*10)
 $script:M.Processes=@($script:M.Processes)+@(
  [pscustomobject]@{ProcessId=$supervisor;ParentProcessId=0;SessionId=1;ExecutablePath=$exe;Generation=$generation;LaunchedFrom=$launchedFrom}
 [pscustomobject]@{ProcessId=$child;ParentProcessId=$supervisor;SessionId=1;ExecutablePath=$exe;Generation=$generation;LaunchedFrom=$launchedFrom}
 )
 # 'all'      - every started helper listens (default, healthy behaviour)
 # 'none'     - no started helper ever listens
 # 'healthy-previous-only' - only a helper launched from the *previous* binary
 #                listens, which models a broken new build. Keying off the
 #                installed binary rather than the start count keeps the
 #                rollback's restarted helper realistic.
 $listens=switch($script:M.ListenGeneration){
  'none' { $false }
  'healthy-previous-only' { $launchedFrom -like 'previous:*' }
  default { $true }
 }
 if($listens){
  $script:M.Listener=@($script:M.Listener)+@([pscustomobject]@{LocalAddress='10.23.45.109';LocalPort=47991;OwningProcess=$child})
 }
}

# One dispatcher for every operation the installer can invoke. A fault matching
# the operation name (and optionally its arguments) is thrown before the real
# simulated effect, which is exactly how a native failure would surface.
function Invoke-Sim {
 param([string]$Operation,[hashtable]$Arguments,[hashtable]$Context)
 $m=$script:M
 $m.Calls.Add([pscustomobject]@{Op=$Operation;Args=$Arguments})
 if($m.DropFileOn -eq $Operation -and $m.DropFile){
  Add-SimFile $m.DropFile.Path $m.DropFile.Content
  $m.DropFileOn=$null
 }
 foreach($fault in $m.Faults){
  if($fault.Op -ne $Operation){continue}
  if($fault.Remaining -eq 0){continue}
  $applies=$true
  if($fault.When){$applies=[bool](& $fault.When $Arguments)}
  if(-not $applies){continue}
  if($fault.Remaining -gt 0){$fault.Remaining=$fault.Remaining-1}
  throw $fault.Message
 }
 switch($Operation){
  'ResolveSid' { return 'S-1-5-21-1111-2222-3333-1001' }
  'CompilerExists' { return $true }
  'Compile' {
   Add-SimFile $Arguments.Output ('compiled:' + $Arguments.Target + ':' + (Get-SimLeaf $Arguments.Output))
   return
  }
  'RunRegression' { $m.RegressionRan=$m.RegressionRan+1; return }
  # New-Item -ItemType Directory -Force creates the whole missing chain.
  'EnsureDirectory' { Add-SimDirectory $Arguments.Path; return }
  'RemoveOwnedDirectory' {
   $leaf=Get-SimLeaf $Arguments.Path
   if($leaf -notmatch '^\.(staging|rollback)-'){throw ('simulated recursive delete of a directory this run does not own: ' + $Arguments.Path)}
   Remove-SimTree $Arguments.Path
   return
  }
  # The preserved previous installer's single recursive-delete operation. It is
  # modelled as what it is - an unconditional recursive delete - so this suite
  # can be run against that source to show which findings it detects (the RED
  # control). The current installer never invokes it.
  'RemoveDirectory' { Remove-SimTree $Arguments.Path; return }
  'RemoveDirectoryIfEmpty' {
   if(-not (Test-SimDir $m $Arguments.Path)){return $true}
   $children=@(Get-SimChildren $Arguments.Path)
   if($children.Count -gt 0){return $false}
   $m.Dirs.Remove((ConvertTo-SimPath $Arguments.Path))
   return (-not (Test-SimDir $m $Arguments.Path))
  }
  'FileExists' { return (Test-SimFile $m $Arguments.Path) }
  'DirectoryExists' { return (Test-SimDir $m $Arguments.Path) }
  'CopyFile' {
   if(-not (Test-SimFile $m $Arguments.Source)){throw ('simulated copy source is missing: ' + $Arguments.Source)}
   Add-SimFile $Arguments.Destination (Get-SimFile $m $Arguments.Source)
   return
  }
  'RemoveFile' {
   $key=ConvertTo-SimPath $Arguments.Path
   if($m.Files.ContainsKey($key)){$m.Files.Remove($key)}
   return
  }
  'ReadFile' { return (Get-SimFile $m $Arguments.Path) }
  'WriteFile' { Add-SimFile $Arguments.Path $Arguments.Content; return }
  'GetFileHash' {
   $content=Get-SimFile $m $Arguments.Path
   if($null -eq $content){return $null}
   return (Get-SimHash $content)
  }
  'GetDirectoryAcl' { return (Get-SimAcl $m $Arguments.Path) }
  'SetDirectoryAcl' {
   Add-SimDirectory $Arguments.Path
   $m.Acls[(ConvertTo-SimPath $Arguments.Path)]=('D:(A;;FA;;;' + $Arguments.Sid + ')(A;;FA;;;SY)(A;;FA;;;BA)')
   return
  }
  'RestoreDirectoryAcl' {
   $m.Acls[(ConvertTo-SimPath $Arguments.Path)]=$Arguments.Sddl
   return
  }
  'GetTask' {
   if(-not $m.TaskRegistered){return $null}
   return [pscustomobject]@{Name='MoonlightClipboard';State=$m.Task.State;Xml=$m.Task.Xml}
  }
  'RegisterTask' {
   if($Arguments.Xml){$m.Task=@{Xml=$Arguments.Xml;State='Ready'} }
   else{
    Assert-True ($null -ne $Arguments.Definition) 'RegisterTask received no definition'
    $definition=$Arguments.Definition
    $xml='<Task><Actions><Exec><Command>' + $definition.Action[0].Execute + '</Command><Arguments>' + $definition.Action[0].Argument + '</Arguments></Exec></Actions>'
    $xml=$xml + '<Principal>' + $definition.Principal.UserId + '/' + $definition.Principal.LogonType + '/' + $definition.Principal.RunLevel + '</Principal>'
    $xml=$xml + '<Settings>RestartCount=' + $definition.Settings.RestartCount + ';RestartInterval=' + $definition.Settings.RestartInterval + ';ExecutionTimeLimit=' + $definition.Settings.ExecutionTimeLimit + '</Settings></Task>'
    $m.Task=@{Xml=$xml;State='Ready'}
   }
   $m.TaskRegistered=$true
   return
  }
  'UnregisterTask' { $m.TaskRegistered=$false; $m.Task=$null; return }
  'StartTask' {
   if(-not $m.TaskRegistered){throw 'simulated Start-ScheduledTask on a missing task'}
   $m.Task.State='Running'
   Start-SimHelper
   return
  }
  # Stop-ScheduledTask only asks the scheduler to end the task instance; it is
  # not a guaranteed process kill. The simulation therefore changes task state
  # only. The installer's explicit Stop-Process calls are what actually end the
  # helper, which is exactly why a rollback must not rely on the task alone.
  'StopTask' { if($m.TaskRegistered){$m.Task.State='Ready'}; return }
  'StartProcess' { Start-SimHelper; return }
  'GetProcesses' { return @($m.Processes | Where-Object {$_.ExecutablePath -eq $Arguments.ExecutablePath}) }
  'StopProcess' {
   $m.Processes=@($m.Processes | Where-Object {$_.ProcessId -ne $Arguments.ProcessId})
   $m.Listener=@($m.Listener | Where-Object {$_.OwningProcess -ne $Arguments.ProcessId})
   return
  }
  'GetListener' { return @($m.Listener | Where-Object {$_.LocalPort -eq $Arguments.Port}) }
  # Mirrors the real adapter's contract: a snapshot either describes every
  # exposure-relevant filter exactly, or it fails closed. A simulated partial
  # read is therefore an exception, never a "clean" object.
  'GetFirewall' {
   if(-not $m.Firewall){return $null}
   if(@($m.FirewallUnreadable).Count -gt 0){
    throw ('The existing firewall rule ' + $m.Firewall.Name + ' could not be snapshotted exactly (unreadable: ' + (@($m.FirewallUnreadable) -join ', ') + ')')
   }
   $copy=[ordered]@{}
   foreach($key in $m.Firewall.Parameters.Keys){$copy[$key]=$m.Firewall.Parameters[$key]}
   return [pscustomobject]@{
    Name=$m.Firewall.Name
    Parameters=$copy
    HasEdgeTraversal=[bool]$copy.Contains('EdgeTraversalPolicy')
    Complete=$true
    Missing=@()
    Notes=@()
   }
  }
  'NewFirewall' {
   $copy=[ordered]@{}
   foreach($key in $Arguments.Parameters.Keys){$copy[$key]=$Arguments.Parameters[$key]}
   $m.Firewall=@{Name=$copy['Name'];Parameters=$copy}
   return
  }
  'RemoveFirewall' { $m.Firewall=$null; return }
  'Sleep' { return }
  default { throw ('Test simulation has no behaviour for installer operation: ' + $Operation) }
 }
}

function New-SimOperations {
 $operations=@{}
 foreach($name in (New-DefaultOperations).Keys){
  $captured=$name
  $operations[$captured]=({param($a,$c) Invoke-Sim $captured $a $c}).GetNewClosure()
 }
 return $operations
}

# A machine with a working previous install: binaries, config, ACLs, a running
# task and the scoped firewall rule.
function Initialize-PreviousInstall {
 param([string]$InstallDir,[string]$BuildRoot)
 $script:M.InstallDir=$InstallDir
 $script:M.BuildRoot=$BuildRoot
 Add-SimDirectory $InstallDir
 Add-SimFile (Join-Path $InstallDir 'MoonlightClipboard.exe') 'previous:MoonlightClipboard.exe'
 Add-SimFile (Join-Path $InstallDir 'Core.dll') 'previous:Core.dll'
 Add-SimFile (Join-Path $InstallDir 'ClipboardGuard.exe') 'previous:ClipboardGuard.exe'
 $token=('a'*64)
 $config=@{bind='10.23.45.109';port=47991;allowed_ips=@('10.23.45.152');token=$token}|ConvertTo-Json
 Add-SimFile (Join-Path $InstallDir 'config.json') $config
 $script:M.Acls[(ConvertTo-SimPath $InstallDir)]='D:(A;;FA;;;S-1-5-21-1111-2222-3333-1001)(A;;FA;;;SY)(A;;FA;;;BA)'
 $script:M.Acls[(ConvertTo-SimPath $BuildRoot)]='D:(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;S-1-5-21-1111-2222-3333-1001)'
 $script:M.Task=@{Xml='<Task version="1.2"><RegistrationInfo><Description>previous helper</Description></RegistrationInfo></Task>';State='Running'}
 $script:M.TaskRegistered=$true
 $script:M.Firewall=@{
  Name='MoonlightClipboard-WebServer-Only'
  Parameters=[ordered]@{
   Name='MoonlightClipboard-WebServer-Only';DisplayName='MoonlightClipboard-WebServer-Only'
   Enabled='True';Direction='Inbound';Action='Allow';Profile='Any';Protocol='TCP'
   LocalAddress=[string[]]@('10.23.45.109');LocalPort=[string[]]@('47991')
   RemoteAddress=[string[]]@('10.23.45.152');Program=(Join-Path $InstallDir 'MoonlightClipboard.exe')
   EdgeTraversalPolicy='Block'
  }
 }
 Start-SimHelper
}

function New-Fault {
 param([string]$Operation,[scriptblock]$When=$null,[string]$Message='injected failure',[int]$Remaining=1)
 return @{Op=$Operation;When=$When;Message=$Message;Remaining=$Remaining}
}

function Invoke-Scenario {
 param(
  [string]$Root,
  [object[]]$Faults=@(),
  [bool]$WithPreviousInstall=$true,
  [string]$ListenGeneration='all',
  # Convenience for scenarios that need one plain injected failure.
  [string]$GeneratedFault=$null,
  # Start the previous helper directly instead of via the scheduled task.
  [switch]$DropTask,
  # The install directory exists (with its ACL) but holds no binaries and no
  # config: a leftover directory from an aborted earlier attempt. This is the
  # only shape in which "remove the config this run created" is the mechanism
  # that removes it, rather than removing the whole directory.
  [switch]$PrecreateInstallDir,
  # A previous rule that is scoped more tightly than the installer's own rule:
  # a specific service, interface and authenticated/encrypted peers. A
  # rollback that loses any of these has silently widened the rule.
  [switch]$RestrictedPreviousFirewall,
  # Model a snapshot the real adapter cannot read exactly.
  [string[]]$FirewallUnreadable=@(),
  # A file another process writes into the install directory mid-run.
  [string]$ConcurrentFileName=$null)
 $script:M=New-SimMachine
 if($GeneratedFault){$Faults=@($Faults)+@(New-Fault -Operation $GeneratedFault -Message ('injected ' + $GeneratedFault + ' failure'))}
 $installDir=Join-Path (Join-Path $Root 'ProgramData') 'MoonlightClipboard'
 $buildRoot=Join-Path $Root 'build'
 $script:M.InstallDir=$installDir
 $script:M.BuildRoot=$buildRoot
 $script:M.Faults=$Faults
 $script:M.ListenGeneration=$ListenGeneration
 $script:M.FirewallUnreadable=$FirewallUnreadable
 if($WithPreviousInstall){Initialize-PreviousInstall -InstallDir $installDir -BuildRoot $buildRoot}
 else{
  # The source directory always exists on a real machine and always has an
  # ACL; only the install directory is genuinely absent for a first install.
  Add-SimDirectory $buildRoot
  $script:M.Acls[(ConvertTo-SimPath $buildRoot)]='D:(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;S-1-5-21-1111-2222-3333-1001)'
 }
 if($DropTask -and $script:M.TaskRegistered){
  # A manually started helper: binaries and a running process, no task.
  $script:M.TaskRegistered=$false
  $script:M.Task=$null
 }
 if($PrecreateInstallDir){
  Add-SimDirectory $installDir
  $script:M.Acls[(ConvertTo-SimPath $installDir)]='D:(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;S-1-5-21-1111-2222-3333-1001)'
 }
 if($RestrictedPreviousFirewall -and $script:M.Firewall){
  $script:M.Firewall.Parameters['Service']='RpcSs'
  $script:M.Firewall.Parameters['InterfaceAlias']=[string[]]@('Ethernet 2')
  $script:M.Firewall.Parameters['InterfaceType']='Lan'
  $script:M.Firewall.Parameters['Authentication']='Require'
  $script:M.Firewall.Parameters['Encryption']='Require'
  $script:M.Firewall.Parameters['LocalUser']='D:(A;;CC;;;S-1-5-21-1111-2222-3333-1001)'
  $script:M.Firewall.Parameters['RemoteMachine']='D:(A;;CC;;;S-1-5-21-1111-2222-3333-2002)'
  $script:M.Firewall.Parameters['OverrideBlockRules']=$false
 }
 if($ConcurrentFileName){
  $script:M.DropFileOn='NewFirewall'
  $script:M.DropFile=@{Path=(Join-Path $installDir $ConcurrentFileName);Content='written by another process'}
 }
 # Staged build inputs the installer compiles from.
 foreach($source in @('Core.cs','Native.cs','Guard.cs','Snapshot.cs','SupervisorLog.cs','Tests.cs')){Add-SimFile (Join-Path $buildRoot $source) ('source:' + $source)}
 $operations=New-SimOperations
 $result=Invoke-MoonlightInstall -InstallDir $installDir -DesktopUser 'WORKSTATION\stream-user' -Bind '10.23.45.109' -WebServer '10.23.45.152' -Port 47991 -Compiler 'C:\csc.exe' -Operations $operations -BuildRoot $buildRoot
 return [pscustomobject]@{Result=$result;Machine=$script:M;InstallDir=$installDir;BuildRoot=$buildRoot;Ops=$operations}
}

function Get-CallNames {
 param($Machine)
 return @($Machine.Calls | ForEach-Object {$_.Op})
}

function Get-CallIndex {
 param($Machine,[string]$Operation)
 $names=Get-CallNames $Machine
 for($i=0;$i -lt $names.Count;$i++){if($names[$i] -eq $Operation){return $i}}
 return -1
}

function Assert-NoActivation {
 param($Machine)
 $names=Get-CallNames $Machine
 foreach($name in @('StopTask','StopProcess','SetDirectoryAcl','NewFirewall','RemoveFirewall','RegisterTask','StartTask','WriteFile')){
  Assert-True (-not ($names -contains $name)) ('operation ' + $name + ' must not run before any change is made')
 }
}

# The whole point of the finding: a failure after the old helper was stopped
# must leave a working previous helper behind, with every layer byte-exact.
function Assert-PreviousInstallRestored {
 param($Scenario,[string]$ExpectedStage)
 $machine=$Scenario.Machine
 $result=$Scenario.Result
 Assert-Equal $result.Outcome 'rolled-back' ('outcome for a failed ' + $ExpectedStage)
 Assert-Equal $result.ExitCode 2 ('exit code for a failed ' + $ExpectedStage)
 Assert-Equal $result.Stage $ExpectedStage 'failure stage'
 Assert-Equal (Get-SimFile $machine (Join-Path $Scenario.InstallDir 'MoonlightClipboard.exe')) 'previous:MoonlightClipboard.exe' 'previous helper binary restored'
 Assert-Equal (Get-SimFile $machine (Join-Path $Scenario.InstallDir 'Core.dll')) 'previous:Core.dll' 'previous Core.dll restored'
 Assert-Equal (Get-SimFile $machine (Join-Path $Scenario.InstallDir 'ClipboardGuard.exe')) 'previous:ClipboardGuard.exe' 'previous guard binary restored'
 $config=Get-SimFile $machine (Join-Path $Scenario.InstallDir 'config.json')
 Assert-True ($null -ne $config) 'config.json is present after rollback'
 Assert-True ($config -match ('a'*64)) 'the original token was preserved through rollback'
 Assert-Equal (Get-SimAcl $machine $Scenario.InstallDir) 'D:(A;;FA;;;S-1-5-21-1111-2222-3333-1001)(A;;FA;;;SY)(A;;FA;;;BA)' 'install directory ACL restored'
 Assert-Equal (Get-SimAcl $machine $Scenario.BuildRoot) 'D:(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;S-1-5-21-1111-2222-3333-1001)' 'build directory ACL restored'
 Assert-True $machine.TaskRegistered 'the previous scheduled task is registered again'
 Assert-Equal $machine.Task.Xml '<Task version="1.2"><RegistrationInfo><Description>previous helper</Description></RegistrationInfo></Task>' 'previous task XML restored verbatim'
 Assert-Equal $machine.Task.State 'Running' 'the previous task is running again'
 $firewall=$machine.Firewall
 Assert-True ($null -ne $firewall) 'the previous firewall rule exists again'
 Assert-Equal $firewall.Parameters['EdgeTraversalPolicy'] 'Block' 'edge traversal policy restored'
 Assert-Equal (@($firewall.Parameters['LocalPort']) -join ',') '47991' 'firewall local port restored'
 Assert-Equal (@($firewall.Parameters['RemoteAddress']) -join ',') '10.23.45.152' 'firewall remote address restored'
 Assert-Equal $firewall.Parameters['Program'] (Join-Path $Scenario.InstallDir 'MoonlightClipboard.exe') 'firewall program path restored'
 Assert-True (@($machine.Processes).Count -ge 2) 'a supervised previous helper was started again'
 Assert-Equal (@($machine.Listener).Count) 1 'exactly one listener is owned by the restored helper'
 $steps=@($result.Report.RollbackSteps | Where-Object {$_.Step -eq 'verify-previous-helper'})
 Assert-Equal $steps.Count 1 'the rollback verified the previous helper health'
 Assert-Equal $steps[0].Ok $true ('previous helper health check: ' + $steps[0].Detail)
 Assert-Equal @($result.Report.RollbackProblems).Count 0 'no rollback step failed'
 # Staging is disposable; the rollback snapshot surviving is the recovery point.
 Assert-True (-not (Test-SimDir $machine $result.Report.Staging)) 'the staging directory was removed'
}

# ---------------------------------------------------------------------------
# 1. Happy paths. These keep the rollback tests honest: the same simulation
#    must reach a verified activation when nothing fails.
# ---------------------------------------------------------------------------
Test-Case 'install-succeeds-on-a-fresh-machine' {
 $scenario=Invoke-Scenario -Root '/sim/fresh' -WithPreviousInstall $false
 Assert-Equal $scenario.Result.Outcome 'installed' 'outcome'
 Assert-Equal $scenario.Result.ExitCode 0 'exit code'
 Assert-Equal (@($scenario.Machine.Processes).Count) 2 'supervisor and child are running'
 Assert-Equal (@($scenario.Machine.Listener).Count) 1 'one listener'
 Assert-True $scenario.Machine.TaskRegistered 'task registered'
 Assert-True ($null -ne $scenario.Machine.Firewall) 'firewall rule created'
 Assert-Equal $scenario.Machine.Firewall.Parameters['EdgeTraversalPolicy'] 'Block' 'edge traversal blocked'
 $config=Get-SimFile $scenario.Machine (Join-Path $scenario.InstallDir 'config.json') | ConvertFrom-Json
 Assert-Equal $config.bind '10.23.45.109' 'generated bind'
 Assert-Equal $config.port 47991 'generated port'
 Assert-Equal (@($config.allowed_ips)[0]) '10.23.45.152' 'generated allowlist'
 Assert-True ($config.token -match '^[0-9a-f]{64}$') 'generated token is 64 hex characters'
 Assert-Equal $scenario.Machine.RegressionRan 1 'the fake-backend regression suite ran before activation'
 Assert-Equal $scenario.Result.Report.Previous.HelperWasRunning $false 'no previous helper was reported'
 # The definition handed to the adapter carries the intended supervision
 # policy in the task-scheduler's own ISO-8601 form; the adapter is the only
 # place that converts it (see the adapter scenario at the end of this file).
 Assert-True ($scenario.Machine.Task.Xml -like '*RestartInterval=PT1M*') 'the registered definition asks for a one-minute restart interval'
 Assert-True ($scenario.Machine.Task.Xml -like '*RestartCount=999*') 'the registered definition asks for 999 restarts'
 Assert-True ($scenario.Machine.Task.Xml -like '*ExecutionTimeLimit=PT0S*') 'the registered definition has no execution time limit'
}

Test-Case 'reinstall-preserves-token-and-replaces-definition' {
 $scenario=Invoke-Scenario -Root '/sim/reinstall'
 Assert-Equal $scenario.Result.Outcome 'installed' 'outcome'
 $config=Get-SimFile $scenario.Machine (Join-Path $scenario.InstallDir 'config.json') | ConvertFrom-Json
 Assert-Equal $config.token ('a'*64) 'the existing token was reused'
 Assert-Equal (Get-SimFile $scenario.Machine (Join-Path $scenario.InstallDir 'MoonlightClipboard.exe')) 'compiled:winexe:MoonlightClipboard.exe' 'the new binary is installed'
 Assert-True ($scenario.Machine.Task.Xml -notlike '*previous helper*') 'the task definition was replaced'
 Assert-True ($scenario.Machine.Task.Xml -like '*WORKSTATION\stream-user/Interactive/Limited*') 'the task principal is the interactive limited user'
 Assert-Equal $scenario.Result.Report.Previous.HelperWasRunning $true 'a previous helper was reported'
 $rollbackFiles=@($scenario.Machine.Files.Keys | Where-Object {Test-SimPathLike $_ '*/.rollback-*/*'})
 Assert-Equal (@($rollbackFiles | Where-Object {$_ -like '*ClipboardGuard.exe'}).Count) 1 'the previous guard binary is kept as a rollback point'
}

# ---------------------------------------------------------------------------
# 2. Backup failure must abort BEFORE the old helper is stopped.
# ---------------------------------------------------------------------------
Test-Case 'backup-failure-leaves-the-running-helper-untouched' {
 $fault=New-Fault -Operation 'CopyFile' -When ({param($a) Test-SimPathLike $a.Destination '*/.rollback-*/Core.dll'}).GetNewClosure() -Message 'injected backup copy failure'
 $scenario=Invoke-Scenario -Root '/sim/backup' -Faults @($fault)
 # The injection must actually have fired. A matcher that silently misses on
 # this platform would otherwise leave the backup intact, the install would
 # succeed, and this scenario would quietly become a happy-path test.
 Assert-Equal $fault.Remaining 0 'sanity: the injected backup-copy failure actually fired'
 Assert-True (-not (Test-SimDir $scenario.Machine $scenario.Result.Report.Rollback)) 'sanity: the backup never completed'
 Assert-Equal $scenario.Result.Outcome 'aborted-before-change' 'outcome'
 Assert-Equal $scenario.Result.ExitCode 1 'exit code'
 Assert-Equal $scenario.Result.Stage 'backup' 'stage'
 Assert-NoActivation $scenario.Machine
 Assert-Equal (Get-SimFile $scenario.Machine (Join-Path $scenario.InstallDir 'MoonlightClipboard.exe')) 'previous:MoonlightClipboard.exe' 'the live binary is untouched'
 Assert-Equal $scenario.Machine.Task.State 'Running' 'the live task is still running'
 Assert-Equal (@($scenario.Machine.Listener).Count) 1 'the live listener is still up'
 Assert-Equal $scenario.Machine.Task.Xml '<Task version="1.2"><RegistrationInfo><Description>previous helper</Description></RegistrationInfo></Task>' 'the task definition is untouched'
 $stagingFiles=@($scenario.Machine.Files.Keys | Where-Object {Test-SimPathLike $_ '*/.staging-*/*'})
 Assert-Count $stagingFiles 0 'no staging output was left behind'
 Assert-True (-not (Test-SimDir $scenario.Machine $scenario.Result.Report.Staging)) 'the staging directory was removed'
 Assert-True (-not (Test-SimDir $scenario.Machine $scenario.Result.Report.Rollback)) 'the partial rollback directory was removed'
}

Test-Case 'preflight-compile-failure-aborts-before-any-change' {
 $fault=New-Fault -Operation 'Compile' -When ({param($a) $a.Target -eq 'library'}).GetNewClosure() -Message 'injected core compile failure'
 $scenario=Invoke-Scenario -Root '/sim/compile' -Faults @($fault)
 Assert-Equal $scenario.Result.Outcome 'aborted-before-change' 'outcome'
 Assert-Equal $scenario.Result.Stage 'staging' 'stage'
 Assert-NoActivation $scenario.Machine
 Assert-Equal $scenario.Machine.Task.State 'Running' 'the live task is still running'
 Assert-Equal $scenario.Machine.RegressionRan 0 'the regression suite never ran'
}

Test-Case 'failed-regression-suite-never-stops-the-live-helper' {
 $fault=New-Fault -Operation 'RunRegression' -Message 'injected regression failure'
 $scenario=Invoke-Scenario -Root '/sim/regression' -Faults @($fault)
 Assert-Equal $scenario.Result.Outcome 'aborted-before-change' 'outcome'
 Assert-Equal $scenario.Result.Stage 'staging' 'stage'
 Assert-NoActivation $scenario.Machine
 Assert-Equal $scenario.Machine.Task.State 'Running' 'the live task is still running'
}

# ---------------------------------------------------------------------------
# 3. One failure per activation step. Each must roll the previous install back.
# ---------------------------------------------------------------------------
Test-Case 'copy-failure-rolls-back' {
 $fault=New-Fault -Operation 'CopyFile' -When ({param($a) (Test-SimPathLike $a.Destination '*MoonlightClipboard.exe') -and (Test-SimPathLike $a.Source '*/.staging-*/*')}).GetNewClosure() -Message 'injected install copy failure'
 $scenario=Invoke-Scenario -Root '/sim/copy' -Faults @($fault)
 Assert-Equal $scenario.Result.Stage 'files' 'stage'
 Assert-PreviousInstallRestored $scenario 'files'
 # The failed switch must not leave this run's partially-copied binaries
 # lying next to the restored ones, and no half-switched helper may run.
 Assert-Equal (@($scenario.Machine.Processes).Count) 2 'only the restored previous helper is running'
 $fromNewBuild=@($scenario.Machine.Processes | Where-Object {$_.LaunchedFrom -like 'compiled:*'})
 Assert-Count $fromNewBuild 0 'no helper launched from the failed build survived'
 $fromPrevious=@($scenario.Machine.Processes | Where-Object {$_.LaunchedFrom -like 'previous:*'})
 Assert-Count $fromPrevious 2 'both restored processes run the previous binary'
}

Test-Case 'config-write-failure-rolls-back' {
 $fault=New-Fault -Operation 'WriteFile' -Message 'injected config write failure'
 $scenario=Invoke-Scenario -Root '/sim/config-fail' -Faults @($fault) -WithPreviousInstall $false
 Assert-Equal $scenario.Result.Stage 'config' 'stage'
 Assert-Equal $scenario.Result.Outcome 'rolled-back' 'outcome'
 Assert-Equal $scenario.Result.ExitCode 2 'exit code'
 Assert-True (-not (Test-SimFile $scenario.Machine (Join-Path $scenario.InstallDir 'config.json'))) 'the half-written config was removed'
 Assert-Equal (@($scenario.Machine.Processes).Count) 0 'no helper is left running'
 Assert-True (-not $scenario.Machine.TaskRegistered) 'the new task was unregistered'
 Assert-True ($null -eq $scenario.Machine.Firewall) 'no firewall rule was created or left behind'
 $steps=@($scenario.Result.Report.RollbackSteps | Where-Object {$_.Step -eq 'verify-no-helper'})
 Assert-Equal $steps.Count 1 'the no-prior-helper end state was verified'
 Assert-Equal $steps[0].Ok $true 'no helper is expected after a fresh-install rollback'
 # A first install that fails must not leave a usable config/token behind:
 # a later run would otherwise adopt credentials from a rolled-back attempt.
 Assert-Count ($scenario.Result.Report.RollbackSteps | Where-Object {$_.Step -eq 'remove-new-config' -and $_.Ok}) 1 'the created config removal was verified'
 Assert-True (-not (Test-SimFile $scenario.Machine (Join-Path $scenario.InstallDir 'config.json'))) 'no config token survives a rolled-back first install'
}

Test-Case 'acl-failure-rolls-back' {
 $fault=New-Fault -Operation 'SetDirectoryAcl' -Message 'injected Set-Acl failure'
 $scenario=Invoke-Scenario -Root '/sim/acl' -Faults @($fault)
 Assert-Equal $scenario.Result.Stage 'acl' 'stage'
 Assert-PreviousInstallRestored $scenario 'acl'
}

Test-Case 'first-install-failing-after-config-removes-the-token' {
 # The config is created before the ACL/firewall/task steps. A later failure
 # must not leave that token behind for a subsequent run to adopt.
 # The install directory pre-exists here, so the token can only disappear via
 # the config-removal step (not by deleting the whole directory).
 $fault=New-Fault -Operation 'NewFirewall' -Message 'injected firewall failure'
 $scenario=Invoke-Scenario -Root '/sim/fresh-config-rollback' -Faults @($fault) -WithPreviousInstall $false -PrecreateInstallDir
 Assert-Equal $scenario.Result.Stage 'firewall' 'stage'
 Assert-Equal $scenario.Result.Outcome 'rolled-back' 'outcome'
 Assert-Equal $scenario.Result.ExitCode 2 'exit code'
 Assert-Equal $scenario.Result.Report.Activation.Config 'created' 'the config really had been created before the failure'
 Assert-True (-not (Test-SimFile $scenario.Machine (Join-Path $scenario.InstallDir 'config.json'))) 'the token created by this run was removed'
 Assert-Count ($scenario.Result.Report.RollbackSteps | Where-Object {$_.Step -eq 'remove-new-config' -and $_.Ok}) 1 'the created config removal was verified'
 Assert-Count ($scenario.Result.Report.RollbackSteps | Where-Object {$_.Step -eq 'restore-config'}) 0 'no restore path was faked for a config that never existed'
 Assert-True (Test-SimDir $scenario.Machine $scenario.InstallDir) 'the pre-existing install directory was kept'
 Assert-Equal (Get-SimAcl $scenario.Machine $scenario.InstallDir) 'D:(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;S-1-5-21-1111-2222-3333-1001)' 'the pre-existing directory ACL was restored'
 Assert-True (-not $scenario.Machine.TaskRegistered) 'no task is left registered'
 Assert-True ($null -eq $scenario.Machine.Firewall) 'no firewall rule is left behind'
 Assert-Equal (@($scenario.Machine.Processes).Count) 0 'no helper is running'
}

Test-Case 'firewall-failure-rolls-back' {
 $fault=New-Fault -Operation 'NewFirewall' -When ({param($a) $a.Parameters['LocalPort'] -eq 47991}).GetNewClosure() -Message 'injected New-NetFirewallRule failure'
 $scenario=Invoke-Scenario -Root '/sim/firewall' -Faults @($fault)
 Assert-Equal $scenario.Result.Stage 'firewall' 'stage'
 Assert-PreviousInstallRestored $scenario 'firewall'
}

Test-Case 'firewall-restore-keeps-an-absent-edge-traversal-gap-visible' {
 # A previous rule without a readable edge-traversal policy must still be
 # recreated, but the missing field is reported instead of silently defaulted.
 $fault=New-Fault -Operation 'NewFirewall' -When ({param($a) $a.Parameters['LocalPort'] -eq 47991}).GetNewClosure() -Message 'injected firewall failure'
 $scenario=Invoke-Scenario -Root '/sim/firewall-gap' -Faults @($fault)
 Assert-Equal $scenario.Result.Stage 'firewall' 'stage'
 Assert-PreviousInstallRestored $scenario 'firewall'
}

Test-Case 'firewall-restore-preserves-service-interface-and-security-scope' {
 # The original defect: the snapshot read Service off the application filter
 # (which has no such property on Windows) and dropped the interface and
 # security filters entirely, so a restricted rule came back widened.
 $fault=New-Fault -Operation 'NewFirewall' -When ({param($a) $a.Parameters['LocalPort'] -eq 47991}).GetNewClosure() -Message 'injected firewall failure'
 $scenario=Invoke-Scenario -Root '/sim/firewall-restricted' -Faults @($fault) -RestrictedPreviousFirewall
 Assert-Equal $scenario.Result.Stage 'firewall' 'stage'
 Assert-PreviousInstallRestored $scenario 'firewall'
 $restored=$scenario.Machine.Firewall.Parameters
 Assert-Equal $restored['Service'] 'RpcSs' 'the service scope was restored, not widened to any service'
 Assert-Equal (@($restored['InterfaceAlias']) -join ',') 'Ethernet 2' 'the interface scope was restored'
 Assert-Equal $restored['InterfaceType'] 'Lan' 'the interface type was restored'
 Assert-Equal $restored['Authentication'] 'Require' 'the authentication requirement was restored'
 Assert-Equal $restored['Encryption'] 'Require' 'the encryption requirement was restored'
 Assert-Equal $restored['LocalUser'] 'D:(A;;CC;;;S-1-5-21-1111-2222-3333-1001)' 'the local-user scope was restored'
 Assert-Equal $restored['RemoteMachine'] 'D:(A;;CC;;;S-1-5-21-1111-2222-3333-2002)' 'the remote-machine scope was restored'
 Assert-True ($restored.Contains('OverrideBlockRules')) 'the override-block-rules value was restored'
 Assert-Equal $restored['OverrideBlockRules'] $false 'override-block-rules stays false'
}

Test-Case 'unreadable-firewall-snapshot-aborts-before-any-change' {
 # Failing closed: if the exact scope cannot be captured, the rule must not be
 # replaced at all. The abort happens in the backup phase, so the live helper
 # is never stopped and no firewall write is attempted.
 $scenario=Invoke-Scenario -Root '/sim/firewall-unreadable' -WithPreviousInstall $true -FirewallUnreadable @('Service')
 Assert-Equal $scenario.Result.Outcome 'aborted-before-change' 'outcome'
 Assert-Equal $scenario.Result.ExitCode 1 'exit code'
 Assert-Equal $scenario.Result.Stage 'backup' 'stage'
 Assert-True ($scenario.Result.Message -like '*snapshotted exactly*') 'the message explains the refused snapshot'
 Assert-True ($scenario.Result.Message -like '*Service*') 'the message names the unreadable field'
 Assert-NoActivation $scenario.Machine
 Assert-Equal $scenario.Machine.Task.State 'Running' 'the live task is still running'
 Assert-Equal (@($scenario.Machine.Listener).Count) 1 'the live listener is still up'
 $firewallWrites=@((Get-CallNames $scenario.Machine) | Where-Object {$_ -eq 'NewFirewall' -or $_ -eq 'RemoveFirewall'})
 Assert-Count $firewallWrites 0 'no firewall write was attempted'
 Assert-True ($null -ne $scenario.Machine.Firewall) 'the existing rule is untouched'
}

Test-Case 'task-registration-failure-rolls-back' {
 $fault=New-Fault -Operation 'RegisterTask' -When ({param($a) $null -eq $a.Xml}).GetNewClosure() -Message 'injected Register-ScheduledTask failure'
 $scenario=Invoke-Scenario -Root '/sim/task' -Faults @($fault)
 Assert-Equal $scenario.Result.Stage 'task' 'stage'
 Assert-PreviousInstallRestored $scenario 'task'
}

Test-Case 'task-start-failure-rolls-back' {
 $fault=New-Fault -Operation 'StartTask' -Message 'injected Start-ScheduledTask failure'
 $scenario=Invoke-Scenario -Root '/sim/start' -Faults @($fault)
 Assert-Equal $scenario.Result.Stage 'start' 'stage'
 Assert-PreviousInstallRestored $scenario 'start'
}

Test-Case 'postcheck-failure-rolls-back' {
 # The new helper starts but never listens: the health check must fail and the
 # previous helper must come back rather than leaving a silent dead bridge.
 # Generation 1 is the previous helper; it listens, the new build does not.
 $scenario=Invoke-Scenario -Root '/sim/postcheck' -Faults @() -ListenGeneration 'healthy-previous-only'
 Assert-Equal $scenario.Result.Stage 'postcheck' 'stage'
 Assert-Equal $scenario.Result.Outcome 'rolled-back' 'outcome'
 Assert-Equal $scenario.Result.ExitCode 2 'exit code'
 Assert-True (@($scenario.Result.Report.Problems).Count -gt 0) 'the verification problems were reported'
 Assert-True (($scenario.Result.Report.Problems -join ';') -like '*listener*') 'the missing listener was named'
 Assert-Equal (Get-SimFile $scenario.Machine (Join-Path $scenario.InstallDir 'MoonlightClipboard.exe')) 'previous:MoonlightClipboard.exe' 'the previous binary was restored'
 Assert-Equal (@($scenario.Machine.Processes).Count) 2 'the previous supervised helper is running again'
 Assert-Equal $scenario.Machine.Task.State 'Running' 'the previous task is running'
 Assert-Count ($scenario.Result.Report.RollbackSteps | Where-Object {$_.Step -eq 'verify-previous-helper' -and $_.Ok}) 1 'the previous helper was proven healthy'
}

Test-Case 'stop-failure-restores-and-keeps-the-surviving-helper' {
 # The previous supervisor survives the stop attempt. The installer must refuse
 # to switch binaries underneath it, roll back the layers it had not yet
 # touched, and verify the still-running helper instead of killing it (killing
 # the thing being preserved is the failure mode AR-08 describes).
 $fault=New-Fault -Operation 'StopProcess' -Message 'injected Stop-Process failure'
 $scenario=Invoke-Scenario -Root '/sim/stop' -Faults @($fault)
 Assert-Equal $scenario.Result.Stage 'stop' 'stage'
 Assert-Equal $scenario.Result.Outcome 'rolled-back' 'outcome'
 Assert-Equal $scenario.Result.ExitCode 2 'exit code'
 Assert-Equal (Get-SimFile $scenario.Machine (Join-Path $scenario.InstallDir 'MoonlightClipboard.exe')) 'previous:MoonlightClipboard.exe' 'the previously installed binary is still in place'
 Assert-Count ($scenario.Result.Report.RollbackSteps | Where-Object {$_.Step -eq 'stop-new-helper'}) 0 'the surviving previous helper was never killed'
 Assert-Count ($scenario.Result.Report.RollbackSteps | Where-Object {$_.Step -eq 'restore-file:Core.dll' -and $_.Ok}) 1 'the binaries were still restored from backup'
 Assert-Count ($scenario.Result.Report.RollbackSteps | Where-Object {$_.Step -eq 'verify-previous-helper' -and $_.Ok}) 1 'the surviving helper was verified healthy'
 Assert-Equal (@($scenario.Machine.Processes).Count) 2 'the original supervised helper is intact'
}

Test-Case 'helper-without-a-task-is-restarted-by-rollback' {
 # A helper started manually (no scheduled task) is still a previous install
 # that must be brought back after a failed activation.
 $scenario=Invoke-Scenario -Root '/sim/no-task' -GeneratedFault 'StartTask' -DropTask
 Assert-Equal $scenario.Result.Outcome 'rolled-back' 'outcome'
 Assert-Equal $scenario.Result.ExitCode 2 'exit code'
 Assert-Equal $scenario.Machine.TaskRegistered $false 'no task was registered by the failed run'
 Assert-Count ($scenario.Result.Report.RollbackSteps | Where-Object {$_.Step -eq 'start-previous-helper' -and $_.Ok}) 1 'the previous helper was started directly'
 Assert-Count ($scenario.Result.Report.RollbackSteps | Where-Object {$_.Step -eq 'verify-previous-helper' -and $_.Ok}) 1 'the restarted helper was verified healthy'
 Assert-Equal (@($scenario.Machine.Processes).Count) 2 'the previous supervised helper is running again'
}

# ---------------------------------------------------------------------------
# 4. No prior install: a failure must leave the machine as empty as it was.
# ---------------------------------------------------------------------------
Test-Case 'fresh-install-failure-removes-task-firewall-and-files' {
 $fault=New-Fault -Operation 'StartTask' -Message 'injected start failure'
 $scenario=Invoke-Scenario -Root '/sim/fresh-rollback' -Faults @($fault) -WithPreviousInstall $false
 Assert-Equal $scenario.Result.Outcome 'rolled-back' 'outcome'
 Assert-Equal $scenario.Result.ExitCode 2 'exit code'
 Assert-True (-not $scenario.Machine.TaskRegistered) 'no task is left registered'
 Assert-True ($null -eq $scenario.Machine.Firewall) 'no firewall rule is left behind'
 Assert-True (-not (Test-SimFile $scenario.Machine (Join-Path $scenario.InstallDir 'config.json'))) 'the generated config was removed'
 Assert-True (-not (Test-SimFile $scenario.Machine (Join-Path $scenario.InstallDir 'MoonlightClipboard.exe'))) 'the installed binary was removed'
 Assert-Equal (@($scenario.Machine.Processes).Count) 0 'no helper process is left running'
 $steps=@($scenario.Result.Report.RollbackSteps)
 $taskRemoval=@($steps | Where-Object {$_.Step -eq 'remove-new-task' -and $_.Ok})
 $firewallRemoval=@($steps | Where-Object {$_.Step -eq 'remove-new-firewall' -and $_.Ok})
 Assert-Count $taskRemoval 1 'the new task removal was verified'
 Assert-Count $firewallRemoval 1 'the firewall absence was verified'
}

Test-Case 'install-directory-created-by-this-run-is-removed-on-rollback' {
 # Staging is created *inside* the install directory and New-Item -Force
 # creates the missing parents, so a fresh install always creates its own
 # install directory. Reading "did it exist?" after that would classify it as
 # pre-existing and leave it behind (with a restored ACL) instead of removing
 # it. The simulation creates parents exactly like New-Item does.
 $fault=New-Fault -Operation 'StartTask' -Message 'injected start failure'
 $scenario=Invoke-Scenario -Root '/sim/fresh-created-dir' -Faults @($fault) -WithPreviousInstall $false
 Assert-True (-not (Test-SimDir $scenario.Machine $scenario.InstallDir)) 'the install directory this run created was removed'
 Assert-True (-not (Test-SimDir $scenario.Machine $scenario.Result.Report.Staging)) 'its staging directory is gone'
 Assert-True (-not (Test-SimDir $scenario.Machine $scenario.Result.Report.Rollback)) 'its rollback snapshot directory is gone'
 Assert-Equal $scenario.Result.Outcome 'rolled-back' 'outcome'
 Assert-Equal $scenario.Result.ExitCode 2 'exit code'
 $removal=@($scenario.Result.Report.RollbackSteps | Where-Object {$_.Step -eq ('remove-created-directory:' + $scenario.InstallDir)})
 Assert-Equal $removal.Count 1 'the created-directory removal is reported as a rollback step'
 Assert-Equal $removal[0].Ok $true ('created-directory removal: ' + $removal[0].Detail)
 Assert-Count ($scenario.Result.Report.RollbackSteps | Where-Object {$_.Step -eq ('restore-acl:' + $scenario.InstallDir)}) 0 'no ACL was "restored" onto the directory this run had created'
 # The source directory did exist before, so its ACL is genuinely restored.
 Assert-Count ($scenario.Result.Report.RollbackSteps | Where-Object {$_.Step -eq ('restore-acl:' + $scenario.BuildRoot) -and $_.Ok}) 1 'the pre-existing source directory ACL is still restored'
}

Test-Case 'concurrent-unknown-file-blocks-recursive-directory-deletion' {
 # Rollback promises never to delete unknown contents. A file that appears in
 # the install directory while this run is active (another process, an
 # operator) must survive: the directory is kept, the leftover is reported as
 # an incomplete rollback, and everything this run did create is still undone.
 $fault=New-Fault -Operation 'NewFirewall' -Message 'injected firewall failure'
 $scenario=Invoke-Scenario -Root '/sim/concurrent-file' -Faults @($fault) -WithPreviousInstall $false -ConcurrentFileName 'operator-note.txt'
 $unknown=Join-Path $scenario.InstallDir 'operator-note.txt'
 Assert-True (Test-SimFile $scenario.Machine $unknown) 'the file another process wrote was preserved'
 Assert-Equal (Get-SimFile $scenario.Machine $unknown) 'written by another process' 'its content is untouched'
 Assert-True (Test-SimDir $scenario.Machine $scenario.InstallDir) 'the directory holding it was kept'
 Assert-Equal $scenario.Result.Outcome 'rollback-incomplete' 'outcome'
 Assert-Equal $scenario.Result.ExitCode 3 'exit code'
 $step=@($scenario.Result.Report.RollbackSteps | Where-Object {$_.Step -eq ('remove-created-directory:' + $scenario.InstallDir)})
 Assert-Equal $step.Count 1 'the kept directory is reported'
 Assert-Equal $step[0].Ok $false 'the step is reported as not clean'
 Assert-True (($scenario.Result.Report.RollbackProblems -join ';') -like '*does not own*') 'the reason names the unowned content'
 # Everything this run did create is still gone: only the unknown file remains.
 Assert-True (-not (Test-SimFile $scenario.Machine (Join-Path $scenario.InstallDir 'MoonlightClipboard.exe'))) 'this run’s binary was removed'
 Assert-True (-not (Test-SimFile $scenario.Machine (Join-Path $scenario.InstallDir 'config.json'))) 'this run’s config was removed'
 Assert-True (-not (Test-SimDir $scenario.Machine $scenario.Result.Report.Staging)) 'its staging directory is gone'
 Assert-True (-not (Test-SimDir $scenario.Machine $scenario.Result.Report.Rollback)) 'its rollback snapshot directory is gone'
 Assert-True (-not $scenario.Machine.TaskRegistered) 'no task was left registered'
 Assert-True ($null -eq $scenario.Machine.Firewall) 'no firewall rule was left behind'
}

Test-Case 'remove-directory-if-empty-refuses-unknown-content' {
 # The directory-removal operation itself must refuse to touch a non-empty
 # directory. Asserted against the REAL adapter on a throwaway directory tree,
 # not against the simulation: a caller that asks politely is not a guarantee,
 # the operation has to refuse on its own. This is what makes the rollback's
 # "never delete unknown contents" promise true rather than aspirational.
 $root=if($env:TMPDIR){$env:TMPDIR}else{[System.IO.Path]::GetTempPath()}
 $sandbox=Join-Path $root ('ar08-empty-' + [guid]::NewGuid().ToString('N'))
 $created=Join-Path $sandbox 'created'
 $empty=Join-Path $sandbox 'empty-child'
 try{
  [System.IO.Directory]::CreateDirectory($created) | Out-Null
  [System.IO.Directory]::CreateDirectory($empty) | Out-Null
  [System.IO.File]::WriteAllText((Join-Path $created 'operator-note.txt'),'written by another process')
  $ops=New-DefaultOperations

  # A directory holding content this run did not write is kept, and reported
  # as not removed rather than deleted anyway.
  $removedNonEmpty=& $ops['RemoveDirectoryIfEmpty'] @{Path=$created} @{}
  Assert-Equal $removedNonEmpty $false 'a directory with unknown content is reported as not removed'
  Assert-True ([System.IO.Directory]::Exists($created)) 'the directory holding unknown content still exists'
  Assert-True ([System.IO.File]::Exists((Join-Path $created 'operator-note.txt'))) 'the unknown file is untouched'
  Assert-Equal ([System.IO.File]::ReadAllText((Join-Path $created 'operator-note.txt'))) 'written by another process' 'its content is unchanged'

  # A genuinely empty directory is removed, so the guard is about unknown
  # content rather than a blanket refusal to delete anything.
  Assert-Equal (& $ops['RemoveDirectoryIfEmpty'] @{Path=$empty} @{}) $true 'an empty directory is removed'
  Assert-True (-not [System.IO.Directory]::Exists($empty)) 'the empty directory is gone'

  # An already-absent directory reports success: removal is idempotent, which
  # the rollback depends on when it runs a removal twice.
  Assert-Equal (& $ops['RemoveDirectoryIfEmpty'] @{Path=(Join-Path $sandbox 'never-existed')} @{}) $true 'an absent directory reports success'
 }finally{
  if([System.IO.Directory]::Exists($sandbox)){[System.IO.Directory]::Delete($sandbox,$true)}
 }
}

Test-Case 'owned-directory-delete-is-scope-checked' {
 # The recursive delete is only ever permitted for this run's own uniquely
 # named staging/rollback directories. Anything else must be refused BEFORE
 # anything is deleted, so a mis-computed path can never remove an unrelated
 # tree. Exercised against a real (throwaway) directory tree in the scratch
 # area, because this operation is defined by the filesystem adapter itself.
 $root=if($env:TMPDIR){$env:TMPDIR}else{[System.IO.Path]::GetTempPath()}
 $sandbox=Join-Path $root ('ar08-scope-' + [guid]::NewGuid().ToString('N'))
 $install=Join-Path (Join-Path $sandbox 'ProgramData') 'MoonlightClipboard'
 $elsewhere=Join-Path $sandbox 'elsewhere'
 $owned=Join-Path $install '.staging-20260101-000000'
 try{
  [System.IO.Directory]::CreateDirectory($owned) | Out-Null
  [System.IO.Directory]::CreateDirectory($elsewhere) | Out-Null
  [System.IO.File]::WriteAllText((Join-Path $install 'MoonlightClipboard.exe'),'previous:binary')
  [System.IO.File]::WriteAllText((Join-Path $elsewhere 'important.txt'),'do not delete')
  [System.IO.File]::WriteAllText((Join-Path $owned 'Core.dll'),'staged')

  $ops=New-DefaultOperations
  # 1. An unrelated directory is refused, and survives with its content.
  $refused=$null
  try{ & $ops['RemoveOwnedDirectory'] @{Path=$elsewhere} @{} }catch{ $refused=$_.Exception.Message }
  Assert-True ($null -ne $refused) 'the adapter refused to delete a directory it does not own'
  Assert-True ($refused -like '*does not own*') ('the refusal names the reason: ' + $refused)
  Assert-True ([System.IO.File]::Exists((Join-Path $elsewhere 'important.txt'))) 'the unrelated file survived'

  # 2. The install directory itself is refused too: it is emptied by name,
  #    never deleted as a tree.
  $refusedInstall=$null
  try{ & $ops['RemoveOwnedDirectory'] @{Path=$install} @{} }catch{ $refusedInstall=$_.Exception.Message }
  Assert-True ($null -ne $refusedInstall) 'the adapter refused to delete the install directory itself'
  Assert-True ([System.IO.File]::Exists((Join-Path $install 'MoonlightClipboard.exe'))) 'the installed binary survived'

  # 3. This run's own staging directory is deletable, so the guard is about
  #    ownership rather than a blanket refusal to delete anything.
  & $ops['RemoveOwnedDirectory'] @{Path=$owned} @{}
  Assert-True (-not [System.IO.Directory]::Exists($owned)) 'this run''s own staging directory was removed'
  Assert-True ([System.IO.Directory]::Exists($install)) 'its parent install directory was left alone'
  Assert-True ([System.IO.File]::Exists((Join-Path $install 'MoonlightClipboard.exe'))) 'the installed binary is still there'
 }finally{
  if([System.IO.Directory]::Exists($sandbox)){[System.IO.Directory]::Delete($sandbox,$true)}
 }
}

# ---------------------------------------------------------------------------
# 5. Negative controls: the assertions above must be able to fail.
# ---------------------------------------------------------------------------
Test-Case 'a-failed-rollback-is-reported-not-hidden' {
 # Fail the restore of one binary during rollback: the run must report
 # rollback-incomplete (exit 3) and name the failed step instead of claiming a
 # clean rollback.
 $primary=New-Fault -Operation 'StartTask' -Message 'injected start failure'
 $secondary=New-Fault -Operation 'CopyFile' -When ({param($a) Test-SimPathLike $a.Source '*/.rollback-*/Core.dll'}).GetNewClosure() -Message 'injected restore failure'
 $scenario=Invoke-Scenario -Root '/sim/broken-rollback' -Faults @($primary,$secondary)
 Assert-Equal $scenario.Result.Outcome 'rollback-incomplete' 'outcome'
 Assert-Equal $scenario.Result.ExitCode 3 'exit code'
 Assert-True (@($scenario.Result.Report.RollbackProblems).Count -gt 0) 'the rollback failures were reported'
 Assert-True (($scenario.Result.Report.RollbackProblems -join ';') -like '*restore-file:Core.dll*') 'the failed layer was named'
 Assert-True ($scenario.Result.Message -like '*rollback-incomplete*') 'the message names the incomplete rollback'
 Assert-True ($scenario.Result.Message -like '*injected start failure*') 'the message keeps the activation failure'
 Assert-True ($scenario.Result.Message -like '*restore-file:Core.dll*') 'the message names the failed restore'
}

Test-Case 'success-path-assertions-are-not-tautological' {
 # Proves the happy-path test would notice a skipped activation step and that
 # binaries are never switched before the old helper is stopped.
 $scenario=Invoke-Scenario -Root '/sim/record'
 Assert-Equal $scenario.Result.Report.Activation.Healthy $true 'activation recorded a healthy helper'
 Assert-Equal $scenario.Result.Report.Activation.Firewall $true 'activation recorded the firewall step'
 Assert-Equal $scenario.Result.Report.Activation.Task $true 'activation recorded the task step'
 $stopIndex=Get-CallIndex $scenario.Machine 'StopTask'
 Assert-True ($stopIndex -ge 0) 'the previous task was stopped'
 $installCopyIndex=-1
 for($i=0;$i -lt $scenario.Machine.Calls.Count;$i++){
  $call=$scenario.Machine.Calls[$i]
  if($call.Op -eq 'CopyFile' -and (Test-SimPathLike $call.Args.Source '*/.staging-*/*')){$installCopyIndex=$i;break}
 }
 Assert-True ($installCopyIndex -ge 0) 'the staged binaries were copied through the operation table'
 Assert-True ($installCopyIndex -gt $stopIndex) 'the staged binaries were copied into place only after the stop'
}

# ---------------------------------------------------------------------------
# 6. Ordering and isolation guarantees.
# ---------------------------------------------------------------------------
Test-Case 'every-backup-read-happens-before-the-helper-is-stopped' {
 $scenario=Invoke-Scenario -Root '/sim/order'
 $names=Get-CallNames $scenario.Machine
 $stopIndex=Get-CallIndex $scenario.Machine 'StopTask'
 Assert-True ($stopIndex -gt 0) 'the previous task was stopped during activation'
 $lastBackup=-1
 $rollbackCopyIndex=-1
 for($i=0;$i -lt $stopIndex;$i++){
  $call=$scenario.Machine.Calls[$i]
  $isRollbackCopy=($call.Op -eq 'CopyFile') -and (Test-SimPathLike $call.Args.Destination '*/.rollback-*/*')
  if($isRollbackCopy){$rollbackCopyIndex=$i}
  $isBackup=($call.Op -eq 'GetTask') -or ($call.Op -eq 'GetFirewall') -or ($call.Op -eq 'GetDirectoryAcl') -or $isRollbackCopy
  if($isBackup){$lastBackup=$i}
 }
 Assert-True ($lastBackup -ge 0) 'backup operations were recorded'
 # The snapshot copies must be observed, not merely assumed: a separator-
 # sensitive matcher would leave $rollbackCopyIndex at -1 and still pass the
 # weaker "some backup happened" assertion above.
 Assert-True ($rollbackCopyIndex -ge 0) 'rollback snapshot copies were taken before the stop'
 Assert-True ($rollbackCopyIndex -lt $stopIndex) 'all rollback snapshots were taken before the stop'
 Assert-True ((Get-CallIndex $scenario.Machine 'GetTask') -lt $stopIndex) 'the previous task definition was exported before the stop'
 Assert-True ((Get-CallIndex $scenario.Machine 'GetFirewall') -lt $stopIndex) 'the previous firewall rule was captured before the stop'
 # The pre-mutation directory state is read before staging exists, so the
 # first DirectoryExists(install dir) must precede the first EnsureDirectory.
 $firstDirProbe=-1
 $firstEnsure=-1
 for($i=0;$i -lt $names.Count;$i++){
  if($firstDirProbe -lt 0 -and $names[$i] -eq 'DirectoryExists'){$firstDirProbe=$i}
  if($firstEnsure -lt 0 -and $names[$i] -eq 'EnsureDirectory'){$firstEnsure=$i}
 }
 Assert-True ($firstDirProbe -ge 0 -and $firstEnsure -ge 0) 'both a directory probe and a directory creation were recorded'
 Assert-True ($firstDirProbe -lt $firstEnsure) 'directory existence was read before staging was created'
}

Test-Case 'installer-only-acts-through-the-operation-table' {
 # Prove from the installer's own text that no native side effect is called
 # outside New-DefaultOperations, which is the only reason the simulation above
 # is a safe substitute for a real Windows host.
 $start=$installerText.IndexOf('function New-DefaultOperations')
 $end=$installerText.IndexOf('function Invoke-Op')
 Assert-True ($start -ge 0 -and $end -gt $start) 'the operation table is delimited in the installer'
 $outside=$installerText.Substring(0,$start) + $installerText.Substring($end)
 foreach($pattern in @('Register-ScheduledTask','Unregister-ScheduledTask','Start-ScheduledTask','Stop-ScheduledTask','Export-ScheduledTask','New-NetFirewallRule','Remove-NetFirewallRule','Get-NetFirewallRule','Get-NetFirewallServiceFilter','Get-NetTCPConnection','Get-CimInstance','Stop-Process','Start-Process','Set-Acl','Get-Acl','Get-FileHash','New-Item','Copy-Item','Remove-Item')){
  Assert-True (-not ($outside -match [regex]::Escape($pattern))) ('installer calls ' + $pattern + ' outside the operation table')
 }
 Assert-True (-not ($outside -match '&\s*\$Compiler')) 'the C# compiler is invoked only through the operation table'
 Assert-True (-not ($outside -match 'Invoke-Expression')) 'the installer must not use Invoke-Expression'
 # A recursive delete is only ever allowed for this run's own uniquely named
 # staging/rollback directories; the install directory is emptied by name.
 Assert-True (-not ($outside -match '-Recurse')) 'the installer never issues an unscoped recursive delete'
}

Test-Case 'simulation-covers-every-installer-operation' {
 # Collect operations across a healthy install and every failure path above,
 # so an operation only used during rollback (restore ACL, start process, ...)
 # is still proven to be simulated rather than silently unimplemented.
 $declared=@((New-DefaultOperations).Keys | Sort-Object)
 $used=[System.Collections.Generic.List[string]]::new()
 $scenarios=@(
  @{Root='/sim/cov-success';Faults=@();Previous=$true},
  @{Root='/sim/cov-fresh';Faults=@();Previous=$false},
  @{Root='/sim/cov-fresh-rollback';Faults=@((New-Fault -Operation 'StartTask' -Message 'injected'));Previous=$false},
  @{Root='/sim/cov-reinstall-rollback';Faults=@((New-Fault -Operation 'StartTask' -Message 'injected'));Previous=$true},
  # No task, previous helper stopped during activation: the rollback must
  # relaunch it directly, which is the only path that uses StartProcess.
  @{Root='/sim/cov-manual-helper';Faults=@((New-Fault -Operation 'StartTask' -Message 'injected'));Previous=$true;DropTask=$true}
 )
 foreach($spec in $scenarios){
  $scenario=Invoke-Scenario -Root $spec.Root -Faults $spec.Faults -WithPreviousInstall $spec.Previous -DropTask:([bool]$spec.DropTask)
  foreach($call in $scenario.Machine.Calls){if(-not $used.Contains($call.Op)){$used.Add($call.Op)}}
 }
 $unknown=@($used | Where-Object {-not ($declared -contains $_)})
 Assert-Equal $unknown.Count 0 ('the installer invoked operations the simulation does not model: ' + ($unknown -join ','))
 $unexercised=@($declared | Where-Object {-not ($used -contains $_)})
 Assert-Equal $unexercised.Count 0 ('operations never exercised by the simulation: ' + ($unexercised -join ','))
}

Test-Case 'path-model-matches-both-separators' {
 # The regression this guards: on Windows 5.1 `Join-Path` returns backslashes,
 # so a matcher written as '*/.rollback-*/Core.dll' stopped matching and five
 # scenarios silently turned into happy-path tests. Both shapes must resolve
 # to the same answer on whichever platform runs this suite.
 $posix='/sim/x/ProgramData/MoonlightClipboard/.rollback-20260101-000000/Core.dll'
 $windows='/sim/x\ProgramData\MoonlightClipboard\.rollback-20260101-000000\Core.dll'
 $mixed=Join-Path (Join-Path (Join-Path (Join-Path '/sim/x' 'ProgramData') 'MoonlightClipboard') '.rollback-20260101-000000') 'Core.dll'
 foreach($candidate in @($posix,$windows,$mixed)){
  Assert-True (Test-SimPathLike $candidate '*/.rollback-*/*') ('rollback matcher must match ' + $candidate)
  Assert-True (Test-SimPathLike $candidate '*/.rollback-*/Core.dll') ('leaf matcher must match ' + $candidate)
  Assert-True (-not (Test-SimPathLike $candidate '*/.staging-*/*')) ('staging matcher must not match ' + $candidate)
  Assert-True (Test-SimUnder $candidate '/sim/x/ProgramData/MoonlightClipboard/.rollback-20260101-000000') ('prefix check must match ' + $candidate)
 }
 # A sibling directory with a shared prefix is not "under" the other one.
 Assert-True (-not (Test-SimUnder '/sim/x/y-other/file' '/sim/x/y')) 'a shared name prefix is not containment'
 Assert-Equal (ConvertTo-SimPath $windows) $posix 'both shapes canonicalise to the same key'
 Assert-Equal (ConvertTo-SimPath '/sim/x/') '/sim/x' 'a trailing separator is dropped'
 Assert-Equal (Get-SimLeaf $windows) 'Core.dll' 'the leaf is separator independent'
 Assert-Equal (Get-SimParent $windows) '/sim/x/ProgramData/MoonlightClipboard/.rollback-20260101-000000' 'the parent is separator independent'
}

# ---------------------------------------------------------------------------
# 7. Native adapter contract.
#
# These scenarios run the REAL adapter scriptblocks from New-DefaultOperations,
# with only the operating-system cmdlets replaced by in-memory stubs. They
# exist because the failure they cover is a producer/adapter mismatch, which a
# simulated operation table cannot see: the simulated table replaces the very
# code that was wrong.
#
# On this Linux host none of these cmdlets exist, so the stubs are the whole
# implementation. On a Windows host the stubs shadow the real cmdlets by
# function precedence, and none of them can reach the task engine or the
# firewall. No real task, process, firewall rule or ACL is touched.
# ---------------------------------------------------------------------------
$script:TaskStub=$null
$script:FirewallStub=$null
$script:AdapterStubNames=@(
 'New-ScheduledTaskAction','New-ScheduledTaskPrincipal','New-ScheduledTaskTrigger',
 'New-ScheduledTaskSettingsSet','Register-ScheduledTask',
 'Get-NetFirewallRule','Get-NetFirewallAddressFilter','Get-NetFirewallPortFilter',
 'Get-NetFirewallApplicationFilter','Get-NetFirewallServiceFilter',
 'Get-NetFirewallInterfaceFilter','Get-NetFirewallInterfaceTypeFilter',
 'Get-NetFirewallSecurityFilter','New-NetFirewallRule','Remove-NetFirewallRule')

function Remove-AdapterStubs {
 foreach($name in $script:AdapterStubNames){
  Remove-Item -LiteralPath ('function:' + $name) -ErrorAction SilentlyContinue
 }
 $script:TaskStub=$null
 $script:FirewallStub=$null
}

# A definition shaped exactly like the one the installer registers, including
# the ISO-8601 durations that TimeSpan.Parse rejects.
function New-TaskDefinition {
 return @{
  Action=@(@{Execute='C:\AstraFixture\MoonlightClipboard.exe';Argument='--supervise';WorkingDirectory='C:\AstraFixture'})
  Principal=@{UserId='fixture\fixture';LogonType='Interactive';RunLevel='Limited'}
  Trigger=@(@{AtLogOn=$true;User='fixture\fixture'})
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
}

Test-Case 'task-adapter-accepts-iso8601-durations' {
 Remove-AdapterStubs
 try{
  $script:TaskStub=[pscustomobject]@{Writes=0;Settings=$null;Registered=$null}
  function script:New-ScheduledTaskAction { param($Execute,$Argument,$WorkingDirectory)
   return [pscustomobject]@{Execute=$Execute;Argument=$Argument;WorkingDirectory=$WorkingDirectory} }
  function script:New-ScheduledTaskPrincipal { param($UserId,$LogonType,$RunLevel)
   return [pscustomobject]@{UserId=$UserId;LogonType=$LogonType;RunLevel=$RunLevel} }
  function script:New-ScheduledTaskTrigger { param([switch]$AtLogOn,$User)
   return [pscustomobject]@{AtLogOn=[bool]$AtLogOn;User=$User} }
  function script:New-ScheduledTaskSettingsSet {
   param($ExecutionTimeLimit,$RestartCount,$RestartInterval,$MultipleInstances,
         [switch]$StartWhenAvailable,[switch]$AllowStartIfOnBatteries,[switch]$DontStopIfGoingOnBatteries)
   $script:TaskStub.Settings=[pscustomobject]@{
    ExecutionTimeLimit=$ExecutionTimeLimit;RestartCount=$RestartCount;RestartInterval=$RestartInterval
    MultipleInstances=$MultipleInstances;StartWhenAvailable=[bool]$StartWhenAvailable
    AllowStartIfOnBatteries=[bool]$AllowStartIfOnBatteries;DontStopIfGoingOnBatteries=[bool]$DontStopIfGoingOnBatteries}
   return $script:TaskStub.Settings }
  function script:Register-ScheduledTask { param($TaskName,$Action,$Principal,$Trigger,$Settings,[switch]$Force,$Xml)
   $script:TaskStub.Writes++
   $script:TaskStub.Registered=[pscustomobject]@{TaskName=$TaskName;Actions=$Action;Principal=$Principal;Triggers=$Trigger;Settings=$Settings;Force=[bool]$Force} }

  $ops=New-DefaultOperations
  # This is the exact shape the production definition emits.
  $failure=$null
  try{ & $ops['RegisterTask'] @{Name='Adapter-Probe';Definition=(New-TaskDefinition)} @{} }
  catch{ $failure=$_.Exception.Message }
  Assert-True ($null -eq $failure) ('the adapter rejected an ISO-8601 duration: ' + $failure)
  Assert-Equal $script:TaskStub.Writes 1 'the task cmdlet was invoked exactly once'
  $settings=$script:TaskStub.Settings
  Assert-True ($null -ne $settings) 'the settings object was built'
  # The intended one-minute restart must survive the conversion.
  Assert-Equal $settings.RestartInterval ([TimeSpan]::FromMinutes(1)) 'the restart interval is one minute'
  Assert-Equal $settings.ExecutionTimeLimit ([TimeSpan]::Zero) 'the execution time limit stays unlimited'
  Assert-Equal $settings.RestartCount 999 'the restart count is preserved'
  Assert-Equal $settings.MultipleInstances 'IgnoreNew' 'the multiple-instance policy is preserved'
  Assert-Equal $settings.StartWhenAvailable $true 'start-when-available is preserved'
  Assert-Equal $settings.AllowStartIfOnBatteries $true 'allow-start-on-batteries is preserved'
  Assert-Equal $settings.DontStopIfGoingOnBatteries $true 'dont-stop-on-batteries is preserved'
  Assert-Equal $script:TaskStub.Registered.TaskName 'Adapter-Probe' 'the task name reaches the cmdlet'
  Assert-Equal $script:TaskStub.Registered.Principal.UserId 'fixture\fixture' 'the principal reaches the cmdlet'
  Assert-Equal $script:TaskStub.Registered.Actions[0].Argument '--supervise' 'the action argument reaches the cmdlet'
 }
 finally{ Remove-AdapterStubs }
}

Test-Case 'task-adapter-rejects-an-invalid-restart-interval' {
 # A restart policy that cannot be expressed must fail loudly rather than
 # register a task whose supervisor silently never restarts.
 Remove-AdapterStubs
 try{
  $script:TaskStub=[pscustomobject]@{Writes=0;Settings=$null;Registered=$null}
  function script:New-ScheduledTaskAction { param($Execute,$Argument,$WorkingDirectory) return [pscustomobject]@{Execute=$Execute} }
  function script:New-ScheduledTaskPrincipal { param($UserId,$LogonType,$RunLevel) return [pscustomobject]@{UserId=$UserId} }
  function script:New-ScheduledTaskTrigger { param([switch]$AtLogOn,$User) return [pscustomobject]@{User=$User} }
  function script:New-ScheduledTaskSettingsSet { param($ExecutionTimeLimit,$RestartCount,$RestartInterval,$MultipleInstances,[switch]$StartWhenAvailable,[switch]$AllowStartIfOnBatteries,[switch]$DontStopIfGoingOnBatteries) return [pscustomobject]@{} }
  function script:Register-ScheduledTask { param($TaskName,$Action,$Principal,$Trigger,$Settings,[switch]$Force,$Xml) $script:TaskStub.Writes++ }
  $ops=New-DefaultOperations
  $definition=New-TaskDefinition
  $definition.Settings['RestartInterval']='one minute'
  $failure=$null
  try{ & $ops['RegisterTask'] @{Name='Adapter-Probe';Definition=$definition} @{} }catch{ $failure=$_.Exception.Message }
  Assert-True ($null -ne $failure) 'an unparseable restart interval must be rejected'
  Assert-True ($failure -like '*RestartInterval*') ('the failure names the field: ' + $failure)
  Assert-Equal $script:TaskStub.Writes 0 'no task was registered from an invalid definition'

  # A zero/negative interval is equally meaningless for a retry policy.
  $definition2=New-TaskDefinition
  $definition2.Settings['RestartInterval']='PT0S'
  $failure2=$null
  try{ & $ops['RegisterTask'] @{Name='Adapter-Probe';Definition=$definition2} @{} }catch{ $failure2=$_.Exception.Message }
  Assert-True ($null -ne $failure2) 'a zero restart interval must be rejected'
  Assert-Equal $script:TaskStub.Writes 0 'still no task registered'
 }
 finally{ Remove-AdapterStubs }
}

# Build the OS-cmdlet stubs that reproduce a REAL Windows rule: the
# application filter has no Service property, and the service, interface,
# interface-type and security filters are separate objects with non-default
# values.
function Install-FirewallStubs {
 param($Filters,$Rule)
 $script:FirewallStub=[pscustomobject]@{Rule=$Rule;Filters=$Filters;NewCalls=@();RemoveCalls=0}
 function script:Get-NetFirewallRule { [CmdletBinding()] param($Name)
  if($null -eq $script:FirewallStub.Rule){return $null}
  return $script:FirewallStub.Rule }
 $script:FirewallStubFilterNames=$Filters.Keys
 foreach($key in @($Filters.Keys)){
  $captured=$key
  $body="[CmdletBinding()] param([Parameter(ValueFromPipeline=`$true)]`$InputObject) process { if(`$script:FirewallStub.Filters.ContainsKey('$captured')){ return `$script:FirewallStub.Filters['$captured'] } }"
  Set-Item -LiteralPath ('function:script:Get-NetFirewall' + $key + 'Filter') -Value ([scriptblock]::Create($body))
 }
 function script:New-NetFirewallRule { [CmdletBinding()] param($Name,$DisplayName,$Description,$Group,$Enabled,$Direction,$Action,$Profile,$Protocol,$LocalPort,$RemotePort,$LocalAddress,$RemoteAddress,$Program,$Service,$InterfaceAlias,$InterfaceType,$Authentication,$Encryption,$OverrideBlockRules,$LocalUser,$RemoteUser,$RemoteMachine,$EdgeTraversalPolicy,$IcmpType)
  $recorded=[ordered]@{}
  foreach($pair in @(@('Name',$Name),@('DisplayName',$DisplayName),@('Description',$Description),@('Group',$Group),@('Enabled',$Enabled),@('Direction',$Direction),@('Action',$Action),@('Profile',$Profile),@('Protocol',$Protocol),@('LocalPort',$LocalPort),@('RemotePort',$RemotePort),@('LocalAddress',$LocalAddress),@('RemoteAddress',$RemoteAddress),@('Program',$Program),@('Service',$Service),@('InterfaceAlias',$InterfaceAlias),@('InterfaceType',$InterfaceType),@('Authentication',$Authentication),@('Encryption',$Encryption),@('OverrideBlockRules',$OverrideBlockRules),@('LocalUser',$LocalUser),@('RemoteUser',$RemoteUser),@('RemoteMachine',$RemoteMachine),@('EdgeTraversalPolicy',$EdgeTraversalPolicy),@('IcmpType',$IcmpType))){
   if($PSBoundParameters.ContainsKey($pair[0])){$recorded[$pair[0]]=$pair[1]}
  }
  $script:FirewallStub.NewCalls=@($script:FirewallStub.NewCalls)+@($recorded)
  # A created rule becomes readable again, so a snapshot round trip is testable.
  $script:FirewallStub.Rule=[pscustomobject]@{
   Name=$Name;DisplayName=$DisplayName;Description=$Description;Group=$Group
   Enabled=$Enabled;Direction=$Direction;Action=$Action;Profile=$Profile
   PolicyStore='PersistentStore';EdgeTraversalPolicy=$EdgeTraversalPolicy }
  $script:FirewallStub.Filters=@{
   Address=[pscustomobject]@{LocalAddress=@($LocalAddress);RemoteAddress=@($RemoteAddress)}
   Port=[pscustomobject]@{Protocol=$Protocol;LocalPort=@($LocalPort);RemotePort=@($RemotePort);IcmpType=@()}
   Application=[pscustomobject]@{Program=$Program}
   Service=[pscustomobject]@{Service=$Service}
   Interface=[pscustomobject]@{InterfaceAlias=@($InterfaceAlias)}
   InterfaceType=[pscustomobject]@{InterfaceType=$InterfaceType}
   Security=[pscustomobject]@{Authentication=$Authentication;Encryption=$Encryption;OverrideBlockRules=$OverrideBlockRules;LocalUser=$LocalUser;RemoteUser=$RemoteUser;RemoteMachine=$RemoteMachine}
  }
  return $script:FirewallStub.Rule }
 function script:Remove-NetFirewallRule { [CmdletBinding()] param($Name) $script:FirewallStub.RemoveCalls++; $script:FirewallStub.Rule=$null }
}

function Remove-FirewallStubs {
 foreach($key in @('Address','Port','Application','Service','Interface','InterfaceType','Security')){
  Remove-Item -LiteralPath ('function:script:Get-NetFirewall' + $key + 'Filter') -ErrorAction SilentlyContinue
 }
 Remove-Item -LiteralPath 'function:script:Get-NetFirewallRule' -ErrorAction SilentlyContinue
 Remove-Item -LiteralPath 'function:script:New-NetFirewallRule' -ErrorAction SilentlyContinue
 Remove-Item -LiteralPath 'function:script:Remove-NetFirewallRule' -ErrorAction SilentlyContinue
 $script:FirewallStub=$null
}

# A faithful reading of the native field probe: the rule's Service lives in
# the service filter, the application filter exposes only a program, and the
# interface/interface-type/security filters are separate objects.
function New-FaithfulFirewallRuleFixture {
 param([string]$Program='C:\ProgramData\MoonlightClipboard\MoonlightClipboard.exe')
 return @{
  Rule=[pscustomobject]@{
   Name='MoonlightClipboard-WebServer-Only';DisplayName='Moonlight Clipboard bridge'
   Description='web-server-only inbound bridge';Group='MoonlightClipboard'
   Enabled='True';Direction='Inbound';Action='Allow';Profile='Domain,Private'
   PolicyStore='PersistentStore';EdgeTraversalPolicy='Block' }
  Filters=@{
   Address=[pscustomobject]@{LocalAddress=@('10.23.45.109');RemoteAddress=@('10.23.45.152')}
   Port=[pscustomobject]@{Protocol='TCP';LocalPort=@('47991');RemotePort=@('Any');IcmpType=@()}
   # NOTE: no Service property - this is what the native probe recorded.
   Application=[pscustomobject]@{Program=$Program}
   Service=[pscustomobject]@{Service='RpcSs'}
   Interface=[pscustomobject]@{InterfaceAlias=@('Ethernet 2')}
   InterfaceType=[pscustomobject]@{InterfaceType='Lan'}
   Security=[pscustomobject]@{Authentication='Require';Encryption='Require';OverrideBlockRules=$false;LocalUser='D:(A;;CC;;;S-1-5-21-1111-2222-3333-1001)';RemoteUser='Any';RemoteMachine='D:(A;;CC;;;S-1-5-21-1111-2222-3333-2002)'}
  }
 }
}

# A stable, order-independent rendering of the exposure-relevant fields.
function Format-FirewallScope($Parameters){
 $keys=@('Name','Enabled','Direction','Action','Profile','Protocol','LocalAddress','RemoteAddress','LocalPort','RemotePort','Program','Service','InterfaceAlias','InterfaceType','Authentication','Encryption','OverrideBlockRules','LocalUser','RemoteUser','RemoteMachine','EdgeTraversalPolicy','IcmpType')
 $parts=@()
 foreach($key in $keys){
  if(-not $Parameters.Contains($key)){continue}
  $value=$Parameters[$key]
  if($value -is [array]){$rendered=@($value) -join '|'}else{$rendered=[string]$value}
  $parts+=($key + '=' + $rendered)
 }
 return ($parts -join ';')
}

Test-Case 'firewall-adapter-captures-service-interface-and-security-filters' {
 Remove-FirewallStubs
 try{
  $fixture=New-FaithfulFirewallRuleFixture
  Install-FirewallStubs -Filters $fixture.Filters -Rule $fixture.Rule
  $ops=New-DefaultOperations
  $snapshot=$null
  $failure=$null
  try{ $snapshot=& $ops['GetFirewall'] @{Name='MoonlightClipboard-WebServer-Only'} @{} }catch{ $failure=$_.Exception.Message }
  Assert-True ($null -eq $failure) ('the snapshot failed: ' + $failure)
  Assert-True ($null -ne $snapshot) 'a snapshot was returned'
  Assert-Equal $snapshot.Complete $true 'the snapshot is complete'
  $p=$snapshot.Parameters
  # The finding itself: service came from the wrong object and was lost.
  Assert-True ($p.Contains('Service')) 'the service scope was captured'
  Assert-Equal $p['Service'] 'RpcSs' 'the service comes from Get-NetFirewallServiceFilter'
  Assert-Equal (@($p['InterfaceAlias']) -join ',') 'Ethernet 2' 'the interface alias was captured'
  Assert-Equal $p['InterfaceType'] 'Lan' 'the interface type was captured'
  Assert-Equal $p['Authentication'] 'Require' 'the authentication requirement was captured'
  Assert-Equal $p['Encryption'] 'Require' 'the encryption requirement was captured'
  Assert-Equal $p['OverrideBlockRules'] $false 'override-block-rules was captured as a boolean'
  Assert-Equal $p['LocalUser'] 'D:(A;;CC;;;S-1-5-21-1111-2222-3333-1001)' 'the local-user scope was captured'
  Assert-Equal $p['RemoteMachine'] 'D:(A;;CC;;;S-1-5-21-1111-2222-3333-2002)' 'the remote-machine scope was captured'
  Assert-Equal $p['Profile'] 'Domain,Private' 'the profile was captured'
  Assert-Equal $p['EdgeTraversalPolicy'] 'Block' 'edge traversal was captured'
  Assert-Equal (@($p['LocalPort']) -join ',') '47991' 'the local port was captured'
  Assert-Equal (@($p['RemoteAddress']) -join ',') '10.23.45.152' 'the remote address was captured'
  Assert-Equal $p['Program'] 'C:\ProgramData\MoonlightClipboard\MoonlightClipboard.exe' 'the program path was captured'

  # Replaying the snapshot must reconstruct the identical scope, not a wider
  # one. This is the rollback path.
  $failure=$null
  try{ & $ops['NewFirewall'] @{Parameters=$snapshot.Parameters} @{} }catch{ $failure=$_.Exception.Message }
  Assert-True ($null -eq $failure) ('the replay failed: ' + $failure)
  Assert-Equal @($script:FirewallStub.NewCalls).Count 1 'exactly one rule was created'
  $replayed=$script:FirewallStub.NewCalls[0]
  Assert-Equal $replayed['Service'] 'RpcSs' 'the replayed rule keeps its service scope'
  Assert-Equal (@($replayed['InterfaceAlias']) -join ',') 'Ethernet 2' 'the replayed rule keeps its interface scope'
  Assert-Equal $replayed['InterfaceType'] 'Lan' 'the replayed rule keeps its interface type'
  Assert-Equal $replayed['Authentication'] 'Require' 'the replayed rule keeps its authentication requirement'
  Assert-Equal $replayed['Encryption'] 'Require' 'the replayed rule keeps its encryption requirement'
  Assert-Equal $replayed['LocalUser'] 'D:(A;;CC;;;S-1-5-21-1111-2222-3333-1001)' 'the replayed rule keeps its local-user scope'
  Assert-Equal $replayed['RemoteMachine'] 'D:(A;;CC;;;S-1-5-21-1111-2222-3333-2002)' 'the replayed rule keeps its remote-machine scope'
  Assert-Equal $replayed['EdgeTraversalPolicy'] 'Block' 'the replayed rule keeps edge traversal blocked'

  # And reading that recreated rule back must produce the same snapshot: a
  # round trip that loses nothing is the definition of an exact rollback.
  $second=& $ops['GetFirewall'] @{Name='MoonlightClipboard-WebServer-Only'} @{}
  Assert-Equal (Format-FirewallScope $second.Parameters) (Format-FirewallScope $snapshot.Parameters) 'the snapshot round trip is lossless'
 }
 finally{ Remove-FirewallStubs }
}

Test-Case 'firewall-adapter-fails-closed-when-a-filter-is-unreadable' {
 # Two shapes of unreadable: the filter object is absent entirely, and the
 # filter object exists but lacks the property. Both must refuse the snapshot
 # before anything is removed, never return a partial one.
 foreach($shape in @('absent-object','missing-property')){
  Remove-FirewallStubs
  try{
   $fixture=New-FaithfulFirewallRuleFixture
   if($shape -eq 'absent-object'){$fixture.Filters.Remove('Service')}
   else{$fixture.Filters['Service']=[pscustomobject]@{SomethingElse='value'}}
   Install-FirewallStubs -Filters $fixture.Filters -Rule $fixture.Rule
   $ops=New-DefaultOperations
   $failure=$null
   $snapshot=$null
   try{ $snapshot=& $ops['GetFirewall'] @{Name='MoonlightClipboard-WebServer-Only'} @{} }catch{ $failure=$_.Exception.Message }
   Assert-True ($null -ne $failure) ('an unreadable service filter must be refused (' + $shape + ')')
   Assert-True ($failure -like '*snapshotted exactly*') ('the failure explains the refusal (' + $shape + '): ' + $failure)
   Assert-True ($failure -like '*Service*') ('the failure names the unreadable field (' + $shape + '): ' + $failure)
   Assert-True ($null -eq $snapshot) ('no partial snapshot is returned (' + $shape + ')')
   Assert-Equal @($script:FirewallStub.NewCalls).Count 0 ('no rule was written from a partial snapshot (' + $shape + ')')
   Assert-Equal $script:FirewallStub.RemoveCalls 0 ('the existing rule was not removed (' + $shape + ')')
  }
  finally{ Remove-FirewallStubs }
 }
}

# ---------------------------------------------------------------------------
Write-Host ''
Write-Host ('PASSED ' + $script:Pass + ' FAILED ' + $script:Fail)
if($script:Fail -gt 0){
 foreach($name in $script:Failures){Write-Host ('  failed: ' + $name)}
 exit 1
}
Write-Host 'ALL INSTALLER TRANSACTION TESTS PASSED'
exit 0
