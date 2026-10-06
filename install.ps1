# StrCu installer: installs or updates StrCu for this Windows user, without administrator rights.
#
#   irm https://github.com/sakirsek/strcu/releases/latest/download/install.ps1 | iex
#
# Running it again updates StrCu: a running StrCu is closed, replaced and started again. Settings stay.
#
# Uninstall: removes StrCu together with its settings (password, paired phones, fingerprints, tunnel token),
# its shortcuts and its start with Windows. The Cloudflare side (tunnel, Access) is left as it is.
#
#   & ([scriptblock]::Create((irm https://github.com/sakirsek/strcu/releases/latest/download/install.ps1))) -Uninstall
#
# Options: -Yes uninstalls without asking; -NoStart does not start StrCu after installing; -From <folder>
# installs strcu.exe and strcu.exe.sha256 from a folder instead of the latest release.
#
# This file is ASCII only: Windows PowerShell reads a downloaded script as Latin-1, so Turkish letters are
# written as {c} {g} {i} {I} {o} {s} {u} ... and put in when the text is shown.

param(
    [switch]$Uninstall,
    [switch]$Yes,
    [switch]$NoStart,
    [string]$From
)

# Run in a child scope: with `irm | iex` nothing set here stays in the user's PowerShell session
& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $repo = 'sakirsek/strcu'
    $dir = Join-Path $env:LOCALAPPDATA 'strcu'
    $exe = Join-Path $dir 'strcu.exe'
    $config = Join-Path $dir 'config.json'
    $startMenu = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\StrCu.lnk'
    # STRCU_DESKTOP: another folder for the desktop shortcut (tests)
    $desktopDir = if ($env:STRCU_DESKTOP) { $env:STRCU_DESKTOP } else { [Environment]::GetFolderPath('Desktop') }
    $desktop = Join-Path $desktopDir 'StrCu.lnk'
    $runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'

    # StrCu's own language if it is set up, else the Windows display language
    $lang = (Get-UICulture).TwoLetterISOLanguageName
    if (Test-Path $config) {
        try { $saved = (Get-Content $config -Raw | ConvertFrom-Json).language; if ($saved) { $lang = $saved } } catch {}
    }
    $turkish = $lang -eq 'tr'

    function Text([string]$en, [string]$tr) {
        if (-not $turkish) { return $en }
        $letters = @(('{c}', 0xE7), ('{C}', 0xC7), ('{g}', 0x11F), ('{G}', 0x11E), ('{i}', 0x131), ('{I}', 0x130),
                     ('{o}', 0xF6), ('{O}', 0xD6), ('{s}', 0x15F), ('{S}', 0x15E), ('{u}', 0xFC), ('{U}', 0xDC))
        foreach ($l in $letters) { $tr = $tr.Replace($l[0], [string][char]$l[1]) }
        $tr
    }
    function Say([string]$en, [string]$tr, [string]$color = 'Gray') { Write-Host (Text $en $tr) -ForegroundColor $color }

    function Same-Path([string]$a, [string]$b) {
        if (-not $a -or -not $b) { return $false }
        [IO.Path]::GetFullPath($a).TrimEnd('\') -ieq [IO.Path]::GetFullPath($b).TrimEnd('\')
    }

    # Only the StrCu installed here: another copy (a second folder, a test) is left running
    function Stop-StrCu {
        $running = @(Get-Process strcu -ErrorAction SilentlyContinue | Where-Object { Same-Path $_.Path $exe })
        if ($running.Count -eq 0) { return $false }
        Say 'Closing the running StrCu...' '{C}al{i}{s}an StrCu kapat{i}l{i}yor...'
        $running | Stop-Process -Force
        $running | Wait-Process -Timeout 15 -ErrorAction SilentlyContinue
        $true
    }

    function Shortcut([string]$path) {
        New-Item -ItemType Directory -Force (Split-Path $path) | Out-Null
        $s = (New-Object -ComObject WScript.Shell).CreateShortcut($path)
        $s.TargetPath = $exe
        $s.WorkingDirectory = $dir
        $s.Description = 'StrCu'
        $s.Save()
    }

    function Points-Here([string]$lnk) {
        (Test-Path $lnk) -and (Same-Path (New-Object -ComObject WScript.Shell).CreateShortcut($lnk).TargetPath $exe)
    }

    function Install-StrCu {
        $tmp = Join-Path ([IO.Path]::GetTempPath()) ("strcu-" + [guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory $tmp | Out-Null
        try {
            $newExe = Join-Path $tmp 'strcu.exe'
            $sum = Join-Path $tmp 'strcu.exe.sha256'
            if ($From) {
                Copy-Item (Join-Path $From 'strcu.exe') $newExe
                Copy-Item (Join-Path $From 'strcu.exe.sha256') $sum
            } else {
                $base = "https://github.com/$repo/releases/latest/download"
                Say "Downloading StrCu from github.com/$repo ..." "StrCu github.com/$repo adresinden indiriliyor..."
                Invoke-WebRequest "$base/strcu.exe" -OutFile $newExe -UseBasicParsing
                Invoke-WebRequest "$base/strcu.exe.sha256" -OutFile $sum -UseBasicParsing
            }
            $want = ((Get-Content $sum -Raw).Trim() -split '\s+')[0]
            # .NET directly: Get-FileHash is missing when Windows PowerShell inherits PowerShell 7's module path
            $sha = [Security.Cryptography.SHA256]::Create()
            $stream = [IO.File]::OpenRead($newExe)
            try { $got = -join ($sha.ComputeHash($stream) | ForEach-Object { $_.ToString('x2') }) } finally { $stream.Dispose() }
            if ($got -ine $want) {
                throw (Text 'The download does not match its SHA-256 checksum; nothing was changed.' `
                    '{I}ndirilen dosya SHA-256 {o}zetiyle tutmuyor; hi{c}bir {s}ey de{g}i{s}tirilmedi.')
            }
            $version = (& $newExe --version) -replace '^strcu\s+', ''
            Say "StrCu $version, SHA-256 verified." "StrCu $version, SHA-256 do{g}ruland{i}." 'Green'

            $update = Test-Path $exe
            New-Item -ItemType Directory -Force $dir | Out-Null
            Stop-StrCu | Out-Null
            # A closed exe can stay locked for a moment
            for ($i = 0; ; $i++) {
                try { Move-Item $newExe $exe -Force; break }
                catch { if ($i -ge 20) { throw }; Start-Sleep -Milliseconds 300 }
            }
            Shortcut $startMenu
            Shortcut $desktop
            if ($update) {
                Say "Updated: $exe" "G{u}ncellendi: $exe" 'Green'
            } else {
                Say "Installed: $exe" "Kuruldu: $exe" 'Green'
                Say 'Shortcuts: Start menu and desktop (StrCu).' 'K{i}sayollar: Ba{s}lat men{u}s{u} ve masa{u}st{u} (StrCu).'
            }
            if ($NoStart) { return }
            Start-Process $exe -WorkingDirectory $dir
            if ($update) {
                Say 'StrCu started again.' 'StrCu yeniden ba{s}lat{i}ld{i}.'
            } else {
                Say 'StrCu opened in a new window; the setup continues there.' 'StrCu yeni pencerede a{c}{i}ld{i}; kurulum orada devam ediyor.'
            }
        } finally {
            Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
        }
    }

    function Uninstall-StrCu {
        if (-not (Test-Path $dir)) {
            Say 'StrCu is not installed.' 'StrCu kurulu de{g}il.'
            return
        }
        Say 'This removes StrCu completely:' 'Bu, StrCu''yu tamamen kald{i}r{i}r:' 'Yellow'
        Say "  $dir (the program and all settings: password, paired phones, fingerprints, tunnel token)" `
            "  $dir (program ve t{u}m ayarlar: parola, e{s}le{s}mi{s} telefonlar, parmak izleri, t{u}nel jetonu)"
        Say '  the shortcuts and the start with Windows' '  k{i}sayollar ve Windows a{c}{i}l{i}nca ba{s}lama'
        Say '  The tunnel and Access in Cloudflare stay; remove them in the dashboard if you like.' `
            '  Cloudflare''daki t{u}nel ve Access kal{i}r; istersen panelden kald{i}r.'
        if (-not $Yes) {
            $answer = Read-Host (Text 'Remove? [y/N]' 'Kald{i}r{i}ls{i}n m{i}? [e/H]')
            if ($answer -notmatch '^(y|yes|e|evet)$') {
                Say 'Nothing was removed.' 'Hi{c}bir {s}ey kald{i}r{i}lmad{i}.'
                return
            }
        }
        Stop-StrCu | Out-Null
        $run = (Get-ItemProperty $runKey -Name StrCu -ErrorAction SilentlyContinue).StrCu
        if ($run -and (Same-Path (($run -split '"')[1]) $exe)) { Remove-ItemProperty $runKey -Name StrCu }
        foreach ($lnk in $startMenu, $desktop) { if (Points-Here $lnk) { Remove-Item $lnk -Force } }
        for ($i = 0; ; $i++) {
            try { Remove-Item $dir -Recurse -Force; break }
            catch { if ($i -ge 20) { throw }; Start-Sleep -Milliseconds 300 }
        }
        Say 'StrCu was removed.' 'StrCu kald{i}r{i}ld{i}.' 'Green'
    }

    try {
        if ($Uninstall) { Uninstall-StrCu } else { Install-StrCu }
    } catch {
        Say "Failed: $($_.Exception.Message)" "Olmad{i}: $($_.Exception.Message)" 'Red'
    }
}
