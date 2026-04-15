$ErrorActionPreference = "Stop"

$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
$MetricsPath = Join-Path $RepoRoot "tmp\network_metrics.jsonl"
$HostLogPath = "C:\Users\jagat\Documents\seedbox\superseedr-config\hosts\desktop-0mtgcbo\logs\app.log"

$previousBackend = $env:SUPERSEEDR_DHT_BACKEND
$previousProbe = $env:SUPERSEEDR_MAINLINE_PROBE

try {
    Remove-Item $MetricsPath -ErrorAction SilentlyContinue
    Remove-Item $HostLogPath -ErrorAction SilentlyContinue

    $env:SUPERSEEDR_DHT_BACKEND = "mainline"
    $env:SUPERSEEDR_MAINLINE_PROBE = "1"

    Write-Host "Cleared:"
    Write-Host "  $MetricsPath"
    Write-Host "  $HostLogPath"
    Write-Host ""
    Write-Host "Starting superseedr with the mainline DHT backend and probe logging..."

    Push-Location $RepoRoot
    try {
        cargo run
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
}
