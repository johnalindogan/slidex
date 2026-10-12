# Unelevated Cast firewall check. Prints status=match, status=partial, or status=nomatch.
# The exe path is a parameter so names like O'Brien are not quoted into the script text.
param(
  [Parameter(Mandatory = $true)][string]$ExePath,
  [Parameter(Mandatory = $true)][string]$MediaName,
  [Parameter(Mandatory = $true)][string]$MdnsName,
  [Parameter(Mandatory = $true)][string]$TcpPorts,
  [Parameter(Mandatory = $true)][string]$UdpPort,
  [string]$Fixture = ''
)

$ErrorActionPreference = 'Stop'

function ConvertTo-ProbePath([string]$Path) {
  if ([string]::IsNullOrWhiteSpace($Path)) { return '' }
  $p = $Path.Trim().Replace('/', '\')
  if ($p.StartsWith('\\?\UNC\', [System.StringComparison]::OrdinalIgnoreCase)) {
    $p = '\\' + $p.Substring(8)
  } elseif ($p.StartsWith('\\?\', [System.StringComparison]::OrdinalIgnoreCase)) {
    $rest = $p.Substring(4)
    if ($rest.Length -ge 2 -and [char]::IsLetter($rest, 0) -and $rest[1] -eq ':') {
      $p = $rest
    }
  }
  return $p.TrimEnd('\')
}

function Test-PrivateProfile([string]$Profile) {
  if ($Profile -eq 'Any' -or $Profile -eq '2') { return $true }
  foreach ($part in ($Profile -split '[,\s]+')) {
    if ($part -eq 'Private' -or $part -eq '2') { return $true }
  }
  return $false
}

function Test-LocalSubnet([string]$Remote) {
  foreach ($part in ($Remote -split '[,\s]+')) {
    if ($part -eq 'LocalSubnet') { return $true }
  }
  return $false
}

function Test-CastRecord($Rec, [string]$Name, [string]$Protocol, [string]$Ports, [string]$Exe) {
  if ($null -eq $Rec) { return $false }
  if ([string]$Rec.name -ne $Name) { return $false }
  $enabled = ($Rec.enabled -eq $true) -or ("$($Rec.enabled)" -eq 'True')
  if (-not $enabled) { return $false }
  $direction = [string]$Rec.direction
  if ($direction -ne 'Inbound' -and $direction -ne 'In') { return $false }
  if ([string]$Rec.action -ne 'Allow') { return $false }
  $prog = (ConvertTo-ProbePath ([string]$Rec.program)).ToLower()
  $want = (ConvertTo-ProbePath $Exe).ToLower()
  if ($prog -ne $want) { return $false }
  if (-not (Test-PrivateProfile ([string]$Rec.profile))) { return $false }
  if (-not (Test-LocalSubnet ([string]$Rec.remote))) { return $false }
  $proto = [string]$Rec.protocol
  $protoOk = ($proto -eq $Protocol) -or ($Protocol -eq 'TCP' -and $proto -eq '6') -or ($Protocol -eq 'UDP' -and $proto -eq '17')
  if (-not $protoOk) { return $false }
  $local = ([string]$Rec.localport) -replace '\s', ''
  $expect = $Ports -replace '\s', ''
  return ($local -eq $expect)
}

function ConvertTo-LiveRecord($Rule, [string]$Name) {
  $app = @($Rule | Get-NetFirewallApplicationFilter)
  $addr = @($Rule | Get-NetFirewallAddressFilter)
  $port = @($Rule | Get-NetFirewallPortFilter)
  $program = ''
  if ($app.Length -gt 0) { $program = [string]$app[0].Program }
  $remote = @()
  foreach ($one in $addr) {
    foreach ($value in @($one.RemoteAddress)) { $remote += [string]$value }
  }
  $protocol = ''
  $local = @()
  if ($port.Length -gt 0) {
    $protocol = $port[0].Protocol.ToString()
    foreach ($value in @($port[0].LocalPort)) { $local += [string]$value }
  }
  [pscustomobject]@{
    name = $Name
    enabled = ($Rule.Enabled.ToString() -eq 'True')
    direction = $Rule.Direction.ToString()
    action = $Rule.Action.ToString()
    profile = $Rule.Profile.ToString()
    program = $program
    remote = ($remote -join ',')
    protocol = $protocol
    localport = ($local -join ',')
  }
}

function Get-CastRecords([string]$MediaName, [string]$MdnsName, [string]$Fixture) {
  if (-not [string]::IsNullOrWhiteSpace($Fixture)) {
    # Windows PowerShell 5.1 sends a JSON array through the pipeline as one
    # object. The parentheses enumerate it into one record per element.
    return @((Get-Content -Raw -LiteralPath $Fixture | ConvertFrom-Json))
  }
  $records = @()
  foreach ($name in @($MediaName, $MdnsName)) {
    foreach ($rule in @(Get-NetFirewallRule -DisplayName $name -ErrorAction SilentlyContinue)) {
      $records += ConvertTo-LiveRecord $rule $name
    }
  }
  return $records
}

try {
  $records = @(Get-CastRecords $MediaName $MdnsName $Fixture)
  $mediaOk = $false
  $mdnsOk = $false
  foreach ($rec in $records) {
    if (Test-CastRecord $rec $MediaName 'TCP' $TcpPorts $ExePath) { $mediaOk = $true }
    if (Test-CastRecord $rec $MdnsName 'UDP' $UdpPort $ExePath) { $mdnsOk = $true }
  }
  if ($mediaOk -and $mdnsOk) { Write-Output 'status=match' }
  elseif ($mediaOk -or $mdnsOk) { Write-Output 'status=partial' }
  else { Write-Output 'status=nomatch' }
} catch {
  Write-Output ("query failed: " + $_.Exception.Message)
  exit 1
}
