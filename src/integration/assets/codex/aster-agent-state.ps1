# installed by aster
# managed by aster; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# ASTER_INTEGRATION_ID=codex
# ASTER_INTEGRATION_VERSION=10

param([string]$Action = "")

if ($Action -ne "session") { exit 0 }
if ($env:ASTER_ENV -ne "1") { exit 0 }
if ([string]::IsNullOrWhiteSpace($env:ASTER_PANE_ID)) { exit 0 }

# Read hook input as raw bytes so UTF-8 payloads decode correctly even when the
# console input code page is not UTF-8 (for example CP949 on Korean Windows).
$stdinReader = New-Object System.IO.StreamReader([Console]::OpenStandardInput(), [System.Text.Encoding]::UTF8)
$inputText = $stdinReader.ReadToEnd()
try {
    $payload = if ([string]::IsNullOrWhiteSpace($inputText)) { $null } else { $inputText | ConvertFrom-Json }
} catch {
    exit 0
}

if ($payload.hook_event_name -and $payload.hook_event_name -ne "SessionStart") { exit 0 }

$sessionId = $payload.session_id
if ([string]::IsNullOrWhiteSpace($sessionId)) { exit 0 }
if ([string]::IsNullOrWhiteSpace($payload.transcript_path)) { exit 0 }
if (-not [string]::IsNullOrWhiteSpace($env:CODEX_THREAD_ID) -and $env:CODEX_THREAD_ID -ne $sessionId) { exit 0 }

$seq = [DateTimeOffset]::UtcNow.Ticks
$aster = if ([string]::IsNullOrWhiteSpace($env:ASTER_BIN_PATH)) { "aster" } else { $env:ASTER_BIN_PATH }
try {
    $args = @(
        "pane",
        "report-agent-session",
        $env:ASTER_PANE_ID,
        "--source",
        "aster:codex",
        "--agent",
        "codex",
        "--seq",
        "$seq",
        "--agent-session-id",
        "$sessionId"
    )
    if ($payload.hook_event_name -eq "SessionStart" -and $payload.source -is [string] -and -not [string]::IsNullOrWhiteSpace($payload.source)) {
        $args += @("--session-start-source", "$($payload.source)")
    }
    & $aster @args 2>$null | Out-Null
} catch {
}
