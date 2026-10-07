# Set up a Windows PC as the stems farm's library machine: it pulls finished
# tracks from the GPU box, adds lyrics, low-res music videos and covers, and
# keeps a catalog. Safe to re-run.
#
#   powershell -ExecutionPolicy Bypass -File setup.ps1 -Root I:\stems
#
# Before the first sync:
#   - stemsd.exe and its DLLs in <Root>\bin (build them on a Mac with
#     scripts/package-windows-worker.sh x64 and copy the folder's contents)
#   - <Root>\farm.json pointing at the GPU box (written here as a template)
#   - this PC's public key (printed below) in the box's ~/.ssh/authorized_keys
param(
    [string]$Root = "I:\stems",
    [int]$EveryMinutes = 10,
    # set everything up but don't schedule the sync yet
    [switch]$NoTask
)
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
# UTF-8 without a byte-order mark (PowerShell 5's -Encoding UTF8 adds one)
function Write-Utf8([string]$Path, [string]$Text) {
    [System.IO.File]::WriteAllText($Path, $Text, (New-Object System.Text.UTF8Encoding $false))
}

foreach ($d in "bin", "farm", "library", "logs", "cache\models") {
    New-Item -ItemType Directory -Force -Path (Join-Path $Root $d) | Out-Null
}
$bin = Join-Path $Root "bin"

# yt-dlp (music videos) and Deno (yt-dlp needs a JavaScript runtime for YouTube)
$ytdlp = Join-Path $bin "yt-dlp.exe"
if (-not (Test-Path $ytdlp)) {
    Write-Host "==> yt-dlp"
    Invoke-WebRequest "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe" -OutFile $ytdlp
} else {
    & $ytdlp -U | Out-Null
}
$deno = Join-Path $bin "deno.exe"
if (-not (Test-Path $deno)) {
    Write-Host "==> deno"
    $zip = Join-Path $env:TEMP "deno.zip"
    Invoke-WebRequest "https://github.com/denoland/deno/releases/latest/download/deno-x86_64-pc-windows-msvc.zip" -OutFile $zip
    Expand-Archive $zip -DestinationPath $bin -Force
    Remove-Item $zip
}

# stems config: everything under $Root (STEMS_HOME), videos capped at 480p
$conf = Join-Path $Root "stems.toml"
if (-not (Test-Path $conf)) {
    Write-Utf8 $conf @"
# stems on the library machine: tracks arrive separated from the GPU box;
# this PC adds lyrics, word timing, music videos and covers.
sink = "null"
port = 7373

[karaoke]
# no separation here (the box does it); word alignment runs on the GPU
backend = "dsp"
device = "directml"

[lyrics]
align = true

[video]
enabled = true
align = true
max_height = 480
ytdlp = '$ytdlp'
"@
}

# ssh key for reaching the GPU box
$key = Join-Path $env:USERPROFILE ".ssh\stems_farm"
if (-not (Test-Path $key)) {
    New-Item -ItemType Directory -Force -Path (Split-Path $key) | Out-Null
    ssh-keygen -q -t ed25519 -N '""' -C "stems-farm@$env:COMPUTERNAME" -f $key
}

$farmJson = Join-Path $Root "farm.json"
if (-not (Test-Path $farmJson)) {
    $json = @{
        cloud    = @{ host = "GPU-BOX-ADDRESS"; port = 22; user = "root"; key = $key; farm = "farm" }
        name     = $env:COMPUTERNAME.ToLower()
        batch_mb = 1500
    } | ConvertTo-Json
    Write-Utf8 $farmJson $json
    Write-Host "edit $farmJson with the GPU box's address"
}

# every few minutes, without a console window
if ($NoTask) {
    Write-Host "(not scheduling the sync: -NoTask)"
} else {
$pythonw = (Get-Command pythonw).Source
$pull = Join-Path $Root "farm\pull.py"
$settings = New-ScheduledTaskSettingsSet -MultipleInstances IgnoreNew -ExecutionTimeLimit (New-TimeSpan -Hours 12) -StartWhenAvailable
# two tasks: collecting songs from the box never waits for video downloads
foreach ($t in @(@{ Name = "stems farm sync"; Args = "--no-finish" }, @{ Name = "stems farm finish"; Args = "--finish-only" })) {
    $action = New-ScheduledTaskAction -Execute $pythonw -Argument "`"$pull`" $($t.Args)" -WorkingDirectory $Root
    $trigger = New-ScheduledTaskTrigger -Once -At (Get-Date).AddMinutes(1) -RepetitionInterval (New-TimeSpan -Minutes $EveryMinutes)
    # S4U: runs whether or not anyone is logged in (needs an admin shell);
    # otherwise only while this user is logged in
    try {
        $principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType S4U -RunLevel Limited
        Register-ScheduledTask -TaskName $t.Name -Action $action -Trigger $trigger -Settings $settings -Principal $principal -Force | Out-Null
    } catch {
        Register-ScheduledTask -TaskName $t.Name -Action $action -Trigger $trigger -Settings $settings -Force | Out-Null
    }
}
}

Write-Host ""
Write-Host "public key for the GPU box's ~/.ssh/authorized_keys:"
Get-Content "$key.pub"
Write-Host ""
if (-not $NoTask) { Write-Host "sync runs every $EveryMinutes minutes (task 'stems farm sync')" }
Write-Host "logs in $Root\logs"
