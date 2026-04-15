# TEMP-BENCHMARK-ONLY: remove this temporary benchmark helper before pushing.
param(
    [ValidateSet("internal", "mainline")]
    [string]$Backend = "internal",
    [int]$Iterations = 3,
    [string]$CorpusPath = "",
    [int]$Concurrency = 16,
    [int]$WarmupRounds = 1,
    [int]$Rounds = 1,
    [int]$Limit = 0,
    [int]$IdleTimeoutMs = 1500,
    [int]$LookupTimeoutMs = 6000,
    [int]$Port = 0,
    [int]$TestnetSize = 0,
    [switch]$TestnetUnseeded,
    [int]$TestnetPeerAnnouncers = 48
)

$ErrorActionPreference = "Stop"

$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
if ([string]::IsNullOrWhiteSpace($CorpusPath)) {
    $CorpusPath = Join-Path $RepoRoot "tmp\dht_benchmark_quick_infohashes.txt"
}
$CorpusBuilder = Join-Path $RepoRoot "scripts\build_dht_benchmark_corpus.py"
$DefaultMetricsSource = Join-Path $RepoRoot "tmp\network_metrics.jsonl"

if (-not (Test-Path $CorpusPath)) {
    if (-not (Test-Path $DefaultMetricsSource)) {
        throw "Corpus '$CorpusPath' does not exist and no fallback metrics file was found at '$DefaultMetricsSource'."
    }
    Write-Host "Building corpus from $DefaultMetricsSource ..."
    & py -3 $CorpusBuilder $DefaultMetricsSource $CorpusPath --limit 64
}

$metrics = New-Object System.Collections.Generic.List[object]
$prefix = Join-Path $RepoRoot "tmp\dht_benchmark_${Backend}_series"
function Get-InternalIpv4Summary {
    param(
        [string]$MetricsPath
    )

    $analysisRaw = & cargo run --quiet -- --json analyze-network-metrics $MetricsPath
    $analysis = $analysisRaw | ConvertFrom-Json
    if (-not $analysis.ok) {
        return $null
    }

    $ipv4 = $analysis.data.internal_dht_family_summaries | Where-Object {
        $_.family -eq "ipv4" -and $_.purpose -eq "lookup"
    } | Select-Object -First 1

    if ($null -eq $ipv4) {
        return $null
    }

    return [pscustomobject]@{
        active_routes_available_avg = [double]$ipv4.active_routes_available_avg
        query_success_avg = [double]$ipv4.query_success_avg
        query_failure_avg = [double]$ipv4.query_failure_avg
        peers_avg = [double]$ipv4.peers_avg
    }
}

Push-Location $RepoRoot
try {
    for ($iteration = 1; $iteration -le $Iterations; $iteration++) {
        $suffix = "{0:D2}" -f $iteration
        $outputPath = "${prefix}_${suffix}.json"
        $metricsPath = "${prefix}_${suffix}.metrics.jsonl"
        Remove-Item $outputPath -ErrorAction SilentlyContinue
        Remove-Item $metricsPath -ErrorAction SilentlyContinue

        $env:SUPERSEEDR_DHT_BACKEND = $Backend
        $env:SUPERSEEDR_NETWORK_METRICS_PATH = $metricsPath
        if ($Backend -eq "mainline") {
            $env:SUPERSEEDR_MAINLINE_PROBE = "1"
        }
        else {
            Remove-Item Env:SUPERSEEDR_MAINLINE_PROBE -ErrorAction SilentlyContinue
        }

        $cargoArgs = @(
            "run",
            "--quiet",
            "--",
            "--json",
            "dht-benchmark",
            $CorpusPath,
            "--backend",
            $Backend,
            "--concurrency",
            $Concurrency,
            "--warmup-rounds",
            $WarmupRounds,
            "--rounds",
            $Rounds,
            "--idle-timeout-ms",
            $IdleTimeoutMs,
            "--lookup-timeout-ms",
            $LookupTimeoutMs,
            "--port",
            $Port
        )
        if ($Limit -gt 0) {
            $cargoArgs += @("--limit", $Limit)
        }
        if ($TestnetSize -gt 0) {
            $cargoArgs += @("--testnet-size", $TestnetSize, "--testnet-peer-announcers", $TestnetPeerAnnouncers)
            if ($TestnetUnseeded) {
                $cargoArgs += "--testnet-unseeded"
            }
        }

        Write-Host "Run $iteration/$Iterations ($Backend)"
        $raw = & cargo @cargoArgs
        $raw | Tee-Object -FilePath $outputPath > $null
        $parsed = $raw | ConvertFrom-Json
        if (-not $parsed.ok) {
            throw "Benchmark run $iteration failed."
        }
        $ipv4Summary = $null
        if ($Backend -eq "internal") {
            $ipv4Summary = Get-InternalIpv4Summary -MetricsPath $metricsPath
        }
        $metrics.Add([pscustomobject]@{
            iteration = $iteration
            first_batch_avg = [double]$parsed.data.first_batch_ms.avg
            first_batch_p95 = [double]$parsed.data.first_batch_ms.p95
            unique_ipv4_avg = [double]$parsed.data.unique_ipv4_per_lookup.avg
            unique_ipv6_avg = [double]$parsed.data.unique_ipv6_per_lookup.avg
            unique_total_avg = [double]$parsed.data.unique_peers_per_lookup.avg
            yielded_lookups = [double]$parsed.data.yielded_lookups
            ipv4_active_routes_avg = if ($null -ne $ipv4Summary) { [double]$ipv4Summary.active_routes_available_avg } else { 0.0 }
            ipv4_query_success_avg = if ($null -ne $ipv4Summary) { [double]$ipv4Summary.query_success_avg } else { 0.0 }
            ipv4_query_failure_avg = if ($null -ne $ipv4Summary) { [double]$ipv4Summary.query_failure_avg } else { 0.0 }
            ipv4_peers_avg = if ($null -ne $ipv4Summary) { [double]$ipv4Summary.peers_avg } else { 0.0 }
        })
        if ($null -ne $ipv4Summary) {
            "{0,2}: first_batch_avg={1,6:N1}ms  unique_ipv4={2,7:N1}  unique_total={3,7:N1}  ipv4_active={4,6:N1}  ipv4_success={5,6:N1}" -f `
                $iteration, `
                $parsed.data.first_batch_ms.avg, `
                $parsed.data.unique_ipv4_per_lookup.avg, `
                $parsed.data.unique_peers_per_lookup.avg, `
                $ipv4Summary.active_routes_available_avg, `
                $ipv4Summary.query_success_avg | Write-Host
        }
        else {
            "{0,2}: first_batch_avg={1,6:N1}ms  unique_ipv4={2,7:N1}  unique_ipv6={3,7:N1}  unique_total={4,7:N1}" -f `
                $iteration, `
                $parsed.data.first_batch_ms.avg, `
                $parsed.data.unique_ipv4_per_lookup.avg, `
                $parsed.data.unique_ipv6_per_lookup.avg, `
                $parsed.data.unique_peers_per_lookup.avg | Write-Host
        }
    }
}
finally {
    Pop-Location
    Remove-Item Env:SUPERSEEDR_DHT_BACKEND -ErrorAction SilentlyContinue
    Remove-Item Env:SUPERSEEDR_MAINLINE_PROBE -ErrorAction SilentlyContinue
    Remove-Item Env:SUPERSEEDR_NETWORK_METRICS_PATH -ErrorAction SilentlyContinue
}

function Get-SeriesSummary {
    param(
        [System.Collections.Generic.List[object]]$Rows,
        [string]$PropertyName
    )

    $values = @($Rows | ForEach-Object { [double]$_.$PropertyName })
    return [pscustomobject]@{
        avg = ($values | Measure-Object -Average).Average
        min = ($values | Measure-Object -Minimum).Minimum
        max = ($values | Measure-Object -Maximum).Maximum
    }
}

$firstBatch = Get-SeriesSummary -Rows $metrics -PropertyName "first_batch_avg"
$ipv4 = Get-SeriesSummary -Rows $metrics -PropertyName "unique_ipv4_avg"
$ipv6 = Get-SeriesSummary -Rows $metrics -PropertyName "unique_ipv6_avg"
$uniqueTotal = Get-SeriesSummary -Rows $metrics -PropertyName "unique_total_avg"
$ipv4ActiveRoutes = Get-SeriesSummary -Rows $metrics -PropertyName "ipv4_active_routes_avg"
$ipv4QuerySuccess = Get-SeriesSummary -Rows $metrics -PropertyName "ipv4_query_success_avg"
$ipv4Peers = Get-SeriesSummary -Rows $metrics -PropertyName "ipv4_peers_avg"

Write-Host ""
Write-Host "Summary ($Backend, $Iterations runs)"
"  first_batch_avg: avg={0:N1} min={1:N1} max={2:N1}" -f $firstBatch.avg, $firstBatch.min, $firstBatch.max | Write-Host
"  unique_ipv4_avg: avg={0:N1} min={1:N1} max={2:N1}" -f $ipv4.avg, $ipv4.min, $ipv4.max | Write-Host
"  unique_ipv6_avg: avg={0:N1} min={1:N1} max={2:N1}" -f $ipv6.avg, $ipv6.min, $ipv6.max | Write-Host
"  unique_total_avg: avg={0:N1} min={1:N1} max={2:N1}" -f $uniqueTotal.avg, $uniqueTotal.min, $uniqueTotal.max | Write-Host
if ($Backend -eq "internal") {
    "  ipv4_active_routes_avg: avg={0:N1} min={1:N1} max={2:N1}" -f $ipv4ActiveRoutes.avg, $ipv4ActiveRoutes.min, $ipv4ActiveRoutes.max | Write-Host
    "  ipv4_query_success_avg: avg={0:N1} min={1:N1} max={2:N1}" -f $ipv4QuerySuccess.avg, $ipv4QuerySuccess.min, $ipv4QuerySuccess.max | Write-Host
    "  ipv4_peers_avg: avg={0:N1} min={1:N1} max={2:N1}" -f $ipv4Peers.avg, $ipv4Peers.min, $ipv4Peers.max | Write-Host
}
