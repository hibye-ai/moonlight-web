param([string]$InstallDir='C:\ProgramData\MoonlightClipboard',[switch]$PurgeConfig)
$ErrorActionPreference='Stop'
$Task='MoonlightClipboard'
$Firewall='MoonlightClipboard-WebServer-Only'
$Failures=@()
if(Get-ScheduledTask -TaskName $Task -ErrorAction SilentlyContinue){
 try{Stop-ScheduledTask -TaskName $Task;Unregister-ScheduledTask -TaskName $Task -Confirm:$false}
 catch{$Failures+=$('task removal failed: ' + $_.Exception.Message)}
}
Get-CimInstance Win32_Process -Filter "Name='MoonlightClipboard.exe'"|Where-Object {$_.ExecutablePath -eq (Join-Path $InstallDir 'MoonlightClipboard.exe')}|ForEach-Object {
 try{Stop-Process -Id $_.ProcessId -Force -ErrorAction Stop}
 catch{$Failures+=$('process ' + $_.ProcessId + ' could not be stopped: ' + $_.Exception.Message)}
}
if(Get-NetFirewallRule -Name $Firewall -ErrorAction SilentlyContinue){
 try{Remove-NetFirewallRule -Name $Firewall}
 catch{$Failures+=$('firewall rule removal failed: ' + $_.Exception.Message)}
}
foreach($Name in @('MoonlightClipboard.exe','Core.dll','ClipboardGuard.exe','supervisor.log','supervisor.log.1','supervisor.log.2')){
 try{Remove-Item -LiteralPath (Join-Path $InstallDir $Name) -Force -ErrorAction SilentlyContinue}
 catch{$Failures+=$('could not remove ' + $Name + ': ' + $_.Exception.Message)}
}
if($PurgeConfig){
 try{Remove-Item -LiteralPath (Join-Path $InstallDir 'config.json') -Force -ErrorAction SilentlyContinue}
 catch{$Failures+=$('could not remove config.json: ' + $_.Exception.Message)}
}
# Never hide a partial removal: report what is actually left behind.
$Remaining=@()
$Remaining+=(Get-CimInstance Win32_Process -Filter "Name='MoonlightClipboard.exe'" -ErrorAction SilentlyContinue|Select-Object ProcessId,ExecutablePath)
$Remaining+=(Get-NetTCPConnection -LocalPort 47991 -State Listen -ErrorAction SilentlyContinue|Select-Object LocalAddress,LocalPort,OwningProcess)
$LeftoverFiles=@(foreach($Name in @('MoonlightClipboard.exe','Core.dll','ClipboardGuard.exe')){$Path=Join-Path $InstallDir $Name;if(Test-Path -LiteralPath $Path){$Path}})
$Result=[pscustomobject]@{
 PurgedConfig=[bool]$PurgeConfig
 Failures=$Failures
 RemainingProcesses=@($Remaining|Where-Object {$_.ProcessId})
 RemainingListener=@(Get-NetTCPConnection -LocalPort 47991 -State Listen -ErrorAction SilentlyContinue)
 RemainingFiles=$LeftoverFiles
 ConfigPreserved=(Test-Path -LiteralPath (Join-Path $InstallDir 'config.json'))
}
$Result|ConvertTo-Json -Depth 4
if($Failures.Count -gt 0 -or $LeftoverFiles.Count -gt 0){throw 'Bridge removal is incomplete; see the report above'}
Write-Output 'Bridge removed; token/config preserved unless -PurgeConfig was explicitly supplied.'
