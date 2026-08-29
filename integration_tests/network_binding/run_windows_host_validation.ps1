# SPDX-FileCopyrightText: 2025 The superseedr Contributors
# SPDX-License-Identifier: GPL-3.0-or-later

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$InterfaceAlias,

    [Parameter(Mandatory = $true)]
    [string]$TcpTarget,

    [Parameter(Mandatory = $true)]
    [string]$UdpTarget,

    [Parameter(Mandatory = $true)]
    [string]$DnsServer,

    [Parameter(Mandatory = $true)]
    [string]$HttpUrl,

    [string]$RedirectUrl,

    [ValidateSet('ipv4', 'ipv6', 'dual')]
    [string]$Family = 'ipv4',

    [ValidateSet(
        'tcp',
        'peer-tcp',
        'udp',
        'dht',
        'utp',
        'udp-tracker',
        'bound-dns',
        'listener',
        'http-general',
        'http-tracker',
        'http-tracker-announce',
        'http-rss',
        'http-web-seed',
        'http-redirect',
        'http-proxy-bypass',
        'any-tcp',
        'any-http'
    )]
    [string[]]$Cases = @(
        'tcp',
        'udp',
        'bound-dns',
        'listener',
        'http-general',
        'http-tracker',
        'http-rss',
        'http-web-seed'
    ),

    [string]$DnsHost = 'example.com',

    [string]$TestBinary,

    [string]$ArtifactRoot,

    [switch]$AllowDisruptive,

    [switch]$AllowAdapterDisable,

    [switch]$RetainPacketCaptures
)

$ErrorActionPreference = 'Stop'
$probeName = 'networking::runtime::tests::windows_host_strict_binding_probe'
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
if (-not $ArtifactRoot) {
    $ArtifactRoot = Join-Path $repoRoot 'integration_tests\artifacts'
}
$runId = Get-Date -Format 'yyyyMMdd-HHmmss'
$artifactDirectory = Join-Path ([IO.Path]::GetFullPath($ArtifactRoot)) "windows-binding-$runId"
New-Item -ItemType Directory -Path $artifactDirectory -Force | Out-Null

function Test-IsAdministrator {
    $principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Assert-PktMonIdle {
    $statusOutput = (& pktmon status 2>&1 | Out-String)
    if ($LASTEXITCODE -ne 0) {
        throw "Could not inspect Packet Monitor status: $statusOutput"
    }
    if ($statusOutput -notmatch '(?i)Packet Monitor is not running') {
        throw 'Packet Monitor is already running. Stop the existing diagnostic session before starting Windows binding validation.'
    }

    $filterOutput = (& pktmon filter list 2>&1 | Out-String)
    if ($LASTEXITCODE -ne 0) {
        throw "Could not inspect Packet Monitor filters: $filterOutput"
    }
    if ($filterOutput -notmatch '(?im)^\s*None\s*$') {
        throw 'Packet Monitor already has filters. Remove or preserve them before starting Windows binding validation.'
    }
}

function Remove-RawPacketArtifactsUnlessRetained {
    if ($RetainPacketCaptures) {
        return
    }

    try {
        Get-ChildItem -LiteralPath $artifactDirectory -Recurse -File -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -in @('capture.etl', 'capture.pcapng', 'capture.txt') } |
            ForEach-Object { Remove-Item -LiteralPath $_.FullName -Force }
    } catch {
        Write-Warning "Could not remove raw packet artifacts: $($_.Exception.Message)"
    }
}

function ConvertTo-Endpoint {
    param([Parameter(Mandatory = $true)][string]$Value)

    try {
        $uri = [Uri]("tcp://$Value")
        if ($uri.Port -le 0) {
            throw 'missing port'
        }
        $address = [Net.IPAddress]::Parse($uri.DnsSafeHost)
        return [Net.IPEndPoint]::new($address, $uri.Port)
    } catch {
        throw "Endpoint '$Value' must be a numeric IP socket address, for example 192.0.2.1:443 or [2001:db8::1]:443."
    }
}

function Get-HttpEndpoint {
    param([Parameter(Mandatory = $true)][string]$Value)

    $uri = [Uri]$Value
    if (-not $uri.IsAbsoluteUri) {
        throw "HTTP URL '$Value' must be absolute."
    }
    $parsedAddress = $null
    if (-not [Net.IPAddress]::TryParse($uri.DnsSafeHost, [ref]$parsedAddress)) {
        throw "HTTP URL '$Value' must use a numeric IP host so packet evidence is attributable."
    }
    $port = if ($uri.IsDefaultPort) {
        if ($uri.Scheme -eq 'https') { 443 } else { 80 }
    } else {
        $uri.Port
    }
    return [Net.IPEndPoint]::new($parsedAddress, $port)
}

function Get-SortedPreferredSources {
    param(
        [Parameter(Mandatory = $true)][uint32]$InterfaceIndex,
        [Parameter(Mandatory = $true)][ValidateSet('IPv4', 'IPv6')][string]$AddressFamily
    )

    return @(Get-NetIPAddress -InterfaceIndex $InterfaceIndex -AddressFamily $AddressFamily `
            -ErrorAction SilentlyContinue |
        Where-Object { $_.AddressState -eq 'Preferred' -and -not $_.SkipAsSource } |
        Sort-Object @{ Expression = {
            ([Net.IPAddress]::Parse($_.IPAddress).GetAddressBytes() |
                ForEach-Object { $_.ToString('X2') }) -join ''
        } })
}

function Find-TestBinary {
    if ($script:TestBinary) {
        $resolved = Resolve-Path -LiteralPath $script:TestBinary
        return $resolved.Path
    }

    Push-Location $repoRoot
    try {
        $candidates = @()
        & cargo test --locked --lib --all-features --no-run --message-format=json |
            ForEach-Object {
                try {
                    $entry = $_ | ConvertFrom-Json -ErrorAction Stop
                    if (
                        $entry.reason -eq 'compiler-artifact' -and
                        $entry.target.name -eq 'superseedr' -and
                        $entry.profile.test -and
                        $entry.executable
                    ) {
                        $candidates += $entry.executable
                    }
                } catch {
                    # Cargo status lines are not JSON and are intentionally ignored.
                }
            }
        if ($LASTEXITCODE -ne 0) {
            throw "Cargo could not build the Windows qualification test binary."
        }
        $candidates = @($candidates | Sort-Object -Unique)
        if ($candidates.Count -ne 1) {
            throw "Expected one Windows qualification test binary, found $($candidates.Count)."
        }
        return [IO.Path]::GetFullPath($candidates[0])
    } finally {
        Pop-Location
    }
}

function Set-ProbeEnvironment {
    param(
        [Parameter(Mandatory = $true)][string]$Case,
        [Parameter(Mandatory = $true)][string]$Identity
    )

    $env:SUPERSEEDR_WINDOWS_CASE = $Case
    $env:SUPERSEEDR_WINDOWS_INTERFACE = $Identity
    $env:SUPERSEEDR_WINDOWS_FAMILY = $Family
    $env:SUPERSEEDR_WINDOWS_TCP_TARGET = $TcpTarget
    $env:SUPERSEEDR_WINDOWS_UDP_TARGET = $UdpTarget
    $env:SUPERSEEDR_WINDOWS_DNS_SERVER = $DnsServer
    $env:SUPERSEEDR_WINDOWS_DNS_HOST = $DnsHost
    $env:SUPERSEEDR_WINDOWS_HTTP_URL = if ($Case -eq 'http-redirect') {
        $RedirectUrl
    } else {
        $HttpUrl
    }
    $env:SUPERSEEDR_WINDOWS_UDP_TRACKER_URL = "udp://$UdpTarget/announce"
    $env:SUPERSEEDR_WINDOWS_HTTP_TRACKER_URL = $HttpUrl
}

function Clear-ProbeEnvironment {
    @(
        'SUPERSEEDR_WINDOWS_CASE',
        'SUPERSEEDR_WINDOWS_INTERFACE',
        'SUPERSEEDR_WINDOWS_FAMILY',
        'SUPERSEEDR_WINDOWS_TCP_TARGET',
        'SUPERSEEDR_WINDOWS_UDP_TARGET',
        'SUPERSEEDR_WINDOWS_DNS_SERVER',
        'SUPERSEEDR_WINDOWS_DNS_HOST',
        'SUPERSEEDR_WINDOWS_HTTP_URL',
        'SUPERSEEDR_WINDOWS_UDP_TRACKER_URL',
        'SUPERSEEDR_WINDOWS_HTTP_TRACKER_URL',
        'SUPERSEEDR_WINDOWS_MARKER_DIRECTORY',
        'SUPERSEEDR_WINDOWS_EXPECTED_ERROR'
    ) | ForEach-Object { Remove-Item "Env:$_" -ErrorAction SilentlyContinue }
}

function Get-CaseEndpoint {
    param([Parameter(Mandatory = $true)][string]$Case)

    switch ($Case) {
        { $_ -in @('tcp', 'peer-tcp', 'any-tcp') } { return [pscustomobject]@{ Endpoint = ConvertTo-Endpoint $TcpTarget; Protocol = 'TCP' } }
        { $_ -in @('udp', 'dht', 'utp', 'udp-tracker') } { return [pscustomobject]@{ Endpoint = ConvertTo-Endpoint $UdpTarget; Protocol = 'UDP' } }
        'bound-dns' { return [pscustomobject]@{ Endpoint = ConvertTo-Endpoint $DnsServer; Protocol = 'UDP' } }
        'http-redirect' { return [pscustomobject]@{ Endpoint = Get-HttpEndpoint $RedirectUrl; Protocol = 'TCP' } }
        { $_ -like 'http-*' -or $_ -eq 'any-http' } { return [pscustomobject]@{ Endpoint = Get-HttpEndpoint $HttpUrl; Protocol = 'TCP' } }
        default { return $null }
    }
}

function Stop-ActiveCapture {
    param([string]$LogPath)

    if ($script:captureRunning) {
        & pktmon stop 2>&1 | Out-File -FilePath $LogPath -Append
        $script:captureRunning = $false
    }
}

function Assert-CaptureEvidence {
    param(
        [Parameter(Mandatory = $true)][string]$Case,
        [Parameter(Mandatory = $true)][string]$TextPath,
        [Parameter(Mandatory = $true)][Net.IPEndPoint]$Endpoint,
        [Parameter(Mandatory = $true)][string[]]$ExpectedSources
    )

    $target = [regex]::Escape("$($Endpoint.Address).$($Endpoint.Port)")
    $outbound = @(Get-Content -LiteralPath $TextPath | Where-Object { $_ -match "> $target`:" })
    $inbound = @(Get-Content -LiteralPath $TextPath | Where-Object {
        $line = $_
        $ExpectedSources | Where-Object {
            $line -match "$target\s+>\s+$([regex]::Escape($_))\.\d+`:"
        }
    })
    if ($outbound.Count -eq 0 -and $inbound.Count -eq 0) {
        throw "Case '$Case' captured neither an outbound packet nor a response on the selected source for $Endpoint."
    }
    foreach ($line in $outbound) {
        $matchesExpectedSource = $false
        foreach ($source in $ExpectedSources) {
            if ($line -match "$([regex]::Escape($source))\.\d+\s+>\s+$target`:") {
                $matchesExpectedSource = $true
                break
            }
        }
        if (-not $matchesExpectedSource) {
            throw "Case '$Case' captured an outbound target packet from an unexpected source: $line"
        }
    }
}

function Invoke-ProbeCase {
    param(
        [Parameter(Mandatory = $true)][string]$Case,
        [Parameter(Mandatory = $true)][string]$Identity,
        [Parameter(Mandatory = $true)][string[]]$ExpectedSources,
        [switch]$ExpectBlocked,
        [string]$ExpectedError
    )

    $caseDirectory = Join-Path $artifactDirectory $Case
    New-Item -ItemType Directory -Path $caseDirectory -Force | Out-Null
    $probeLog = Join-Path $caseDirectory 'probe.log'
    $captureLog = Join-Path $caseDirectory 'capture.log'
    $etlPath = Join-Path $caseDirectory 'capture.etl'
    $textPath = Join-Path $caseDirectory 'capture.txt'
    $pcapPath = Join-Path $caseDirectory 'capture.pcapng'
    $caseEndpoint = Get-CaseEndpoint $Case
    $script:captureRunning = $false

    try {
        if ($caseEndpoint) {
            & pktmon filter remove 2>&1 | Out-File -FilePath $captureLog -Append
            if ($caseEndpoint.Protocol -eq 'UDP') {
                # Windows Packet Monitor's endpoint filter can omit valid UDP
                # datagrams on virtual adapters and very small uTP packets.
                # Capture UDP broadly, then attribute the endpoint and selected
                # source from the converted trace.
                & pktmon filter add "Binding-$Case" -t $caseEndpoint.Protocol 2>&1 |
                    Out-File -FilePath $captureLog -Append
            } else {
                & pktmon filter add "Binding-$Case" -i $caseEndpoint.Endpoint.Address `
                    -t $caseEndpoint.Protocol -p $caseEndpoint.Endpoint.Port 2>&1 |
                    Out-File -FilePath $captureLog -Append
            }
            if ($LASTEXITCODE -ne 0) {
                throw "Could not install the packet filter for case '$Case'."
            }
            & pktmon start --capture --comp nics --pkt-size 0 --file-name $etlPath `
                --file-size 32 --log-mode circular 2>&1 |
                Out-File -FilePath $captureLog -Append
            if ($LASTEXITCODE -ne 0) {
                throw "Could not start packet capture for case '$Case'."
            }
            $script:captureRunning = $true
            Start-Sleep -Seconds 1
        }

        Set-ProbeEnvironment -Case $(if ($ExpectBlocked) { 'activation-blocked' } else { $Case }) `
            -Identity $Identity
        if ($ExpectedError) {
            $env:SUPERSEEDR_WINDOWS_EXPECTED_ERROR = $ExpectedError
        }
        if ($Case -eq 'http-proxy-bypass') {
            $oldHttpProxy = $env:HTTP_PROXY
            $oldHttpsProxy = $env:HTTPS_PROXY
            $env:HTTP_PROXY = 'http://127.0.0.1:1'
            $env:HTTPS_PROXY = 'http://127.0.0.1:1'
        }

        & $script:testExecutable $probeName --ignored --exact --nocapture 2>&1 |
            Tee-Object -FilePath $probeLog |
            Out-Host
        $probeExit = $LASTEXITCODE
        if ($probeExit -ne 0) {
            throw "Windows production probe case '$Case' failed with exit code $probeExit."
        }
        if (-not (Select-String -LiteralPath $probeLog -SimpleMatch "WINDOWS_BINDING_PROBE case=$($env:SUPERSEEDR_WINDOWS_CASE)" -Quiet)) {
            throw "Windows production probe case '$Case' did not emit its completion marker."
        }
    } finally {
        if ($Case -eq 'http-proxy-bypass') {
            $env:HTTP_PROXY = $oldHttpProxy
            $env:HTTPS_PROXY = $oldHttpsProxy
        }
        Clear-ProbeEnvironment
        Stop-ActiveCapture -LogPath $captureLog
        & pktmon filter remove 2>&1 | Out-File -FilePath $captureLog -Append
    }

    if ($caseEndpoint) {
        & pktmon etl2txt $etlPath --out $textPath --verbose 3 2>&1 |
            Out-File -FilePath $captureLog -Append
        if ($LASTEXITCODE -ne 0) {
            throw "Could not convert the packet trace for case '$Case' to text."
        }
        & pktmon etl2pcap $etlPath --out $pcapPath 2>&1 |
            Out-File -FilePath $captureLog -Append
        if ($LASTEXITCODE -ne 0) {
            throw "Could not convert the packet trace for case '$Case' to pcapng."
        }
        if (-not (Select-String -LiteralPath $captureLog -SimpleMatch '(No events lost)' -Quiet)) {
            throw "Packet Monitor did not confirm lossless event capture for case '$Case'."
        }
        Assert-CaptureEvidence -Case $Case -TextPath $textPath `
            -Endpoint $caseEndpoint.Endpoint -ExpectedSources $ExpectedSources
    }

    return [pscustomobject]@{
        case = $Case
        result = 'passed'
        blocked = [bool]$ExpectBlocked
        capture = [bool]$caseEndpoint
    }
}

function Wait-ForMarker {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [int]$TimeoutSeconds = 45
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while (-not (Test-Path -LiteralPath $Path)) {
        if ((Get-Date) -ge $deadline) {
            throw "Timed out waiting for marker '$Path'."
        }
        Start-Sleep -Milliseconds 200
    }
}

function Wait-ForAdapterUp {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [int]$TimeoutSeconds = 45
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-NetAdapter -Name $Name -ErrorAction SilentlyContinue).Status -ne 'Up') {
        if ((Get-Date) -ge $deadline) {
            throw "Adapter '$Name' did not return to the Up state."
        }
        Start-Sleep -Milliseconds 500
    }
}

function Invoke-RecoveryCase {
    param(
        [Parameter(Mandatory = $true)][string]$Identity,
        [Parameter(Mandatory = $true)][string[]]$ExpectedSources,
        [Parameter(Mandatory = $true)][string]$AdapterName
    )

    $case = 'recovery'
    $caseDirectory = Join-Path $artifactDirectory $case
    $markerDirectory = Join-Path $caseDirectory 'markers'
    New-Item -ItemType Directory -Path $markerDirectory -Force | Out-Null
    $stdoutPath = Join-Path $caseDirectory 'probe.stdout.log'
    $stderrPath = Join-Path $caseDirectory 'probe.stderr.log'
    $captureLog = Join-Path $caseDirectory 'capture.log'
    $etlPath = Join-Path $caseDirectory 'capture.etl'
    $textPath = Join-Path $caseDirectory 'capture.txt'
    $pcapPath = Join-Path $caseDirectory 'capture.pcapng'
    $endpoint = ConvertTo-Endpoint $TcpTarget
    $readyMarker = Join-Path $markerDirectory 'ready.marker'
    $blockedMarker = Join-Path $markerDirectory 'blocked.marker'
    $recoveredMarker = Join-Path $markerDirectory 'recovered.marker'
    $recoveredSourceMarker = Join-Path $markerDirectory 'recovered-source.marker'
    $script:captureRunning = $false
    $adapterDisabled = $false
    $probeProcess = $null

    try {
        & pktmon filter remove 2>&1 | Out-File -FilePath $captureLog -Append
        & pktmon filter add 'Binding-Recovery' -i $endpoint.Address -t TCP -p $endpoint.Port 2>&1 |
            Out-File -FilePath $captureLog -Append
        if ($LASTEXITCODE -ne 0) {
            throw 'Could not install the recovery packet filter.'
        }
        & pktmon start --capture --comp nics --pkt-size 192 --file-name $etlPath `
            --file-size 32 --log-mode circular 2>&1 |
            Out-File -FilePath $captureLog -Append
        if ($LASTEXITCODE -ne 0) {
            throw 'Could not start the recovery packet capture.'
        }
        $script:captureRunning = $true
        Start-Sleep -Seconds 1

        Set-ProbeEnvironment -Case $case -Identity $Identity
        $env:SUPERSEEDR_WINDOWS_MARKER_DIRECTORY = $markerDirectory
        $probeProcess = Start-Process -FilePath $script:testExecutable `
            -ArgumentList @($probeName, '--ignored', '--exact', '--nocapture') `
            -RedirectStandardOutput $stdoutPath -RedirectStandardError $stderrPath `
            -WindowStyle Hidden -PassThru
        Wait-ForMarker -Path $readyMarker

        Disable-NetAdapter -Name $AdapterName -Confirm:$false
        $adapterDisabled = $true
        Wait-ForMarker -Path $blockedMarker
        Enable-NetAdapter -Name $AdapterName -Confirm:$false
        $adapterDisabled = $false
        Wait-ForAdapterUp -Name $AdapterName
        Wait-ForMarker -Path $recoveredMarker -TimeoutSeconds 75
        Wait-ForMarker -Path $recoveredSourceMarker -TimeoutSeconds 5

        $exited = $probeProcess.WaitForExit(75000)
        if (-not $exited -or -not $probeProcess.HasExited) {
            throw 'Windows recovery probe did not exit after publishing recovery.'
        }
        # Complete redirected-stream draining and refresh the native exit code.
        $probeProcess.WaitForExit()
        $probeProcess.Refresh()
        $probeExitCode = "$($probeProcess.ExitCode)"
        if ($probeExitCode -and [int]$probeExitCode -ne 0) {
            throw "Windows recovery probe failed with exit code $probeExitCode."
        }
        if (-not (Select-String -LiteralPath $stdoutPath -SimpleMatch 'WINDOWS_BINDING_PROBE case=recovery' -Quiet)) {
            throw 'Windows recovery probe did not emit its completion marker.'
        }
        if (-not (Select-String -LiteralPath $stdoutPath -SimpleMatch 'test result: ok.' -Quiet)) {
            throw 'Windows recovery probe did not report a successful test result.'
        }
    } finally {
        if ($adapterDisabled) {
            Enable-NetAdapter -Name $AdapterName -Confirm:$false -ErrorAction Continue
            Wait-ForAdapterUp -Name $AdapterName -TimeoutSeconds 60
        }
        if ($probeProcess -and -not $probeProcess.HasExited) {
            $probeProcess.Kill()
        }
        Clear-ProbeEnvironment
        Stop-ActiveCapture -LogPath $captureLog
        & pktmon filter remove 2>&1 | Out-File -FilePath $captureLog -Append
    }

    & pktmon etl2txt $etlPath --out $textPath --verbose 3 2>&1 |
        Out-File -FilePath $captureLog -Append
    if ($LASTEXITCODE -ne 0) {
        throw 'Could not convert the recovery trace to text.'
    }
    & pktmon etl2pcap $etlPath --out $pcapPath 2>&1 |
        Out-File -FilePath $captureLog -Append
    if ($LASTEXITCODE -ne 0) {
        throw 'Could not convert the recovery trace to pcapng.'
    }
    if (-not (Select-String -LiteralPath $captureLog -SimpleMatch '(No events lost)' -Quiet)) {
        throw 'Packet Monitor did not confirm lossless event capture during recovery.'
    }
    $recoveredSources = @((Get-Content -LiteralPath $recoveredSourceMarker -Raw).Trim())
    $captureSources = @($ExpectedSources + $recoveredSources | Sort-Object -Unique)
    Assert-CaptureEvidence -Case $case -TextPath $textPath `
        -Endpoint $endpoint -ExpectedSources $captureSources

    return [pscustomobject]@{
        case = $case
        result = 'passed'
        blocked = $true
        capture = $true
        generations = (Get-Content -LiteralPath $recoveredMarker -Raw).Trim()
    }
}

if (-not (Test-IsAdministrator)) {
    throw 'Run the Windows host validation harness from an Administrator PowerShell session.'
}
foreach ($command in @('cargo', 'git', 'pktmon')) {
    if (-not (Get-Command $command -ErrorAction SilentlyContinue)) {
        throw "Missing required command '$command'."
    }
}
if ($Cases -contains 'http-redirect' -and -not $RedirectUrl) {
    throw 'RedirectUrl is required when the http-redirect case is selected.'
}
if ($AllowAdapterDisable -and -not $AllowDisruptive) {
    throw 'AllowAdapterDisable also requires AllowDisruptive.'
}
Assert-PktMonIdle
Write-Warning 'This harness briefly captures network packets. Raw ETL, PCAPNG, and decoded packet text are deleted by default.'
if ($RetainPacketCaptures) {
    Write-Warning 'Raw packet retention is enabled. Artifacts may contain unrelated or sensitive network traffic and must be reviewed before sharing.'
}

$adapter = Get-NetAdapter -Name $InterfaceAlias -ErrorAction Stop
if ($adapter.Status -ne 'Up') {
    throw "Selected adapter '$InterfaceAlias' is not up."
}
$adapterRecord = Get-CimInstance Win32_NetworkAdapter |
    Where-Object { $_.InterfaceIndex -eq $adapter.ifIndex } |
    Select-Object -First 1
if (-not $adapterRecord.GUID) {
    throw "Could not resolve the stable AdapterName identity for '$InterfaceAlias'."
}
$interfaceIdentity = $adapterRecord.GUID

$ipv4Sources = Get-SortedPreferredSources -InterfaceIndex $adapter.ifIndex -AddressFamily IPv4
$ipv6Sources = Get-SortedPreferredSources -InterfaceIndex $adapter.ifIndex -AddressFamily IPv6
$expectedSources = @()
if ($Family -in @('ipv4', 'dual')) {
    if ($ipv4Sources.Count -eq 0) {
        throw "Selected adapter '$InterfaceAlias' has no preferred IPv4 source."
    }
    $expectedSources += $ipv4Sources[0].IPAddress
}
if ($Family -in @('ipv6', 'dual')) {
    if ($ipv6Sources.Count -eq 0) {
        throw "Selected adapter '$InterfaceAlias' has no preferred IPv6 source."
    }
    $expectedSources += $ipv6Sources[0].IPAddress
}

$script:testExecutable = Find-TestBinary
$listedTests = & $script:testExecutable --list --format terse
if ($LASTEXITCODE -ne 0 -or $listedTests -notcontains "$probeName`: test") {
    throw "The compiled test binary does not contain '$probeName'."
}

$metadata = [ordered]@{
    commit = (& git -C $repoRoot rev-parse HEAD).Trim()
    windows = [Environment]::OSVersion.VersionString
    elevated = $true
    interface_alias = $InterfaceAlias
    interface_identity = $interfaceIdentity
    interface_index = $adapter.ifIndex
    interface_description = $adapter.InterfaceDescription
    family = $Family
    selected_sources = $expectedSources
    weak_host_policy = @(Get-NetIPInterface -InterfaceIndex $adapter.ifIndex |
        Select-Object AddressFamily, WeakHostSend, WeakHostReceive, ConnectionState)
    default_routes = @(Get-NetRoute |
        Where-Object { $_.DestinationPrefix -in @('0.0.0.0/0', '::/0') } |
        Select-Object AddressFamily, InterfaceIndex, InterfaceAlias, RouteMetric)
    cases = $Cases
    disruptive = [bool]$AllowDisruptive
    adapter_disable = [bool]$AllowAdapterDisable
    packet_captures_retained = [bool]$RetainPacketCaptures
}
$metadata | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $artifactDirectory 'metadata.json')

$results = @()
try {
    foreach ($case in $Cases) {
        $results += Invoke-ProbeCase -Case $case -Identity $interfaceIdentity `
            -ExpectedSources $expectedSources
    }

    if ($AllowDisruptive) {
        $temporaryAlias = "Binding-$runId"
        $renamed = $false
        try {
            Rename-NetAdapter -Name $InterfaceAlias -NewName $temporaryAlias
            $renamed = $true
            $renamedAdapter = Get-NetAdapter -Name $temporaryAlias
            $renamedRecord = Get-CimInstance Win32_NetworkAdapter |
                Where-Object { $_.InterfaceIndex -eq $renamedAdapter.ifIndex } |
                Select-Object -First 1
            if ($renamedRecord.GUID -ne $interfaceIdentity) {
                throw 'Stable Windows adapter identity changed after a friendly-name rename.'
            }
            $results += Invoke-ProbeCase -Case 'identity-rename' `
                -Identity $interfaceIdentity -ExpectedSources $expectedSources
        } finally {
            if ($renamed) {
                Rename-NetAdapter -Name $temporaryAlias -NewName $InterfaceAlias
            }
        }

        foreach ($policyCase in @(
            @{ Name = 'weak-host-send'; Property = 'WeakHostSend'; Error = 'WeakHostSend' },
            @{ Name = 'weak-host-receive'; Property = 'WeakHostReceive'; Error = 'WeakHostReceive' }
        )) {
            $ipInterfaces = @(Get-NetIPInterface -InterfaceIndex $adapter.ifIndex |
                Where-Object { $_.AddressFamily.ToString().ToLowerInvariant() -eq $Family -or $Family -eq 'dual' })
            foreach ($ipInterface in $ipInterfaces) {
                $originalSend = $ipInterface.WeakHostSend
                $originalReceive = $ipInterface.WeakHostReceive
                try {
                    $arguments = @{
                        InterfaceIndex = $adapter.ifIndex
                        AddressFamily = $ipInterface.AddressFamily
                        Confirm = $false
                    }
                    $arguments[$policyCase.Property] = 'Enabled'
                    Set-NetIPInterface @arguments
                    $results += Invoke-ProbeCase -Case $policyCase.Name `
                        -Identity $interfaceIdentity -ExpectedSources $expectedSources `
                        -ExpectBlocked -ExpectedError $policyCase.Error
                } finally {
                    Set-NetIPInterface -InterfaceIndex $adapter.ifIndex `
                        -AddressFamily $ipInterface.AddressFamily `
                        -WeakHostSend $originalSend -WeakHostReceive $originalReceive `
                        -Confirm:$false
                }
            }
        }

        if ($AllowAdapterDisable) {
            $currentAdapterName = (Get-NetAdapter |
                Where-Object { $_.ifIndex -eq $adapter.ifIndex } |
                Select-Object -First 1).Name
            $results += Invoke-RecoveryCase -Identity $interfaceIdentity `
                -ExpectedSources $expectedSources -AdapterName $currentAdapterName
        }
    }
} catch {
    $failureSummary = [ordered]@{
        result = 'failed'
        commit = $metadata.commit
        artifact_directory = $artifactDirectory
        completed_cases = $results
        error = ($_ | Out-String)
    }
    $failureSummary | ConvertTo-Json -Depth 5 |
        Set-Content -LiteralPath (Join-Path $artifactDirectory 'summary.json')
    throw
} finally {
    Clear-ProbeEnvironment
    Stop-ActiveCapture -LogPath (Join-Path $artifactDirectory 'cleanup.log')
    & pktmon filter remove 2>&1 | Out-File -FilePath (Join-Path $artifactDirectory 'cleanup.log') -Append
    Remove-RawPacketArtifactsUnlessRetained
}

$summary = [ordered]@{
    result = 'passed'
    commit = $metadata.commit
    artifact_directory = $artifactDirectory
    cases = $results
}
$summary | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $artifactDirectory 'summary.json')
$summary | ConvertTo-Json -Depth 5
