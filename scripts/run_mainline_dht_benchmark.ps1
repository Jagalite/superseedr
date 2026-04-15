# TEMP-BENCHMARK-ONLY: remove this temporary benchmark helper before pushing.
param(
    [string]$CorpusPath = "",
    [int]$Concurrency = 16,
    [int]$WarmupRounds = 1,
    [int]$Rounds = 1,
    [int]$IdleTimeoutMs = 1500,
    [int]$LookupTimeoutMs = 6000,
    [int]$Port = 0,
    [string]$OutputPath = ""
)

$ErrorActionPreference = "Stop"

$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
if ([string]::IsNullOrWhiteSpace($CorpusPath)) {
    $CorpusPath = Join-Path $RepoRoot "tmp\dht_benchmark_infohashes.txt"
}
if ([string]::IsNullOrWhiteSpace($OutputPath)) {
    $OutputPath = Join-Path $RepoRoot "tmp\dht_benchmark_mainline.json"
}
$MetricsPath = Join-Path $RepoRoot "tmp\dht_benchmark_mainline.metrics.jsonl"
$CorpusBuilder = Join-Path $RepoRoot "scripts\build_dht_benchmark_corpus.py"
$DefaultMetricsSource = Join-Path $RepoRoot "tmp\network_metrics.jsonl"

$previousBackend = $env:SUPERSEEDR_DHT_BACKEND
$previousProbe = $env:SUPERSEEDR_MAINLINE_PROBE
$previousMetricsPath = $env:SUPERSEEDR_NETWORK_METRICS_PATH

try {
    if (-not (Test-Path $CorpusPath)) {
        if (-not (Test-Path $DefaultMetricsSource)) {
            throw "Corpus '$CorpusPath' does not exist and no fallback metrics file was found at '$DefaultMetricsSource'."
        }
        Write-Host "Building corpus from $DefaultMetricsSource ..."
        & py -3 $CorpusBuilder $DefaultMetricsSource $CorpusPath
    }

    Remove-Item $OutputPath -ErrorAction SilentlyContinue
    Remove-Item $MetricsPath -ErrorAction SilentlyContinue

    $env:SUPERSEEDR_DHT_BACKEND = "mainline"
    $env:SUPERSEEDR_NETWORK_METRICS_PATH = $MetricsPath
    Remove-Item Env:SUPERSEEDR_MAINLINE_PROBE -ErrorAction SilentlyContinue

    $cargoArgs = @(
        "run",
        "--quiet",
        "--",
        "--json",
        "dht-benchmark",
        $CorpusPath,
        "--backend",
        "mainline",
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

    Write-Host "Running mainline DHT benchmark..."
    Write-Host "  corpus: $CorpusPath"
    Write-Host "  output: $OutputPath"
    Write-Host "  metrics: $MetricsPath"
    Write-Host ""

    Push-Location $RepoRoot
    try {
        & cargo @cargoArgs | Tee-Object -FilePath $OutputPath
    }
    finally {
        Pop-Location
    }
}
finally {
    if ($null -ne $previousBackend) {
        $env:SUPERSEEDR_DHT_BACKEND = $previousBackend
    }
    else {
        Remove-Item Env:SUPERSEEDR_DHT_BACKEND -ErrorAction SilentlyContinue
    }

    if ($null -ne $previousProbe) {
        $env:SUPERSEEDR_MAINLINE_PROBE = $previousProbe
    }
    else {
        Remove-Item Env:SUPERSEEDR_MAINLINE_PROBE -ErrorAction SilentlyContinue
    }

    if ($null -ne $previousMetricsPath) {
        $env:SUPERSEEDR_NETWORK_METRICS_PATH = $previousMetricsPath
    }
    else {
        Remove-Item Env:SUPERSEEDR_NETWORK_METRICS_PATH -ErrorAction SilentlyContinue
    }
}
