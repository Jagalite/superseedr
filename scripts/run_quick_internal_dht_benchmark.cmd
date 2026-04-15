@echo off
setlocal

pushd "%~dp0\.." >nul || exit /b 1

set "CORPUS=tmp\dht_benchmark_quick_infohashes.txt"
set "OUTPUT=tmp\dht_benchmark_internal_quick.json"
set "METRICS=tmp\dht_benchmark_internal_quick.metrics.jsonl"

if not exist "%CORPUS%" (
  if not exist "tmp\network_metrics.jsonl" (
    echo Missing tmp\network_metrics.jsonl and no quick corpus exists.
    popd >nul
    exit /b 1
  )
  echo Building quick corpus from tmp\network_metrics.jsonl ...
  py -3 scripts\build_dht_benchmark_corpus.py tmp\network_metrics.jsonl "%CORPUS%" --limit 64
  if errorlevel 1 (
    popd >nul
    exit /b %errorlevel%
  )
)

if exist "%OUTPUT%" del /q "%OUTPUT%"
if exist "%METRICS%" del /q "%METRICS%"

set "SUPERSEEDR_DHT_BACKEND=internal"
set "SUPERSEEDR_NETWORK_METRICS_PATH=%METRICS%"

echo Running quick internal DHT benchmark...
echo   corpus: %CORPUS%
echo   output: %OUTPUT%
echo   metrics: %METRICS%
echo.

cargo run --quiet -- --json dht-benchmark "%CORPUS%" --backend internal --concurrency 16 --warmup-rounds 1 --rounds 1 --idle-timeout-ms 1500 --lookup-timeout-ms 6000 --port 0 > "%OUTPUT%"
set "EXITCODE=%ERRORLEVEL%"

if %EXITCODE% equ 0 (
  type "%OUTPUT%"
)

popd >nul
exit /b %EXITCODE%
