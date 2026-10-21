# StrCu installer: installs or updates StrCu on this computer as a Windows service.
#
#   irm https://github.com/sakirsek/strcu/releases/latest/download/install.ps1 | iex
#
# It asks for administrator approval once. StrCu 2.0 runs as a Windows service with the rights of the
# computer, which is what lets the phone reach the lock screen, the sign-in screen, the "do you want to allow
# this app" prompt and windows that run as administrator; your own apps, files and clipboard are still
# handled as you. The program goes to %ProgramFiles%\StrCu and the settings to %ProgramData%\StrCu, which
# only administrators may write to.
#
# Running it again updates StrCu: the service is stopped, the program replaced, the service started again.
# Settings stay. An older StrCu's settings are copied over the first time, so nothing has to be set up again;
# its own files are left where they are, to go back to if anything goes wrong.
#
# Uninstall removes the service, the program, its settings (password, paired phones, fingerprints, tunnel
# token), the way through the firewall and the shortcuts. The Cloudflare side (tunnel, Access) stays.
#
#   & ([scriptblock]::Create((irm https://github.com/sakirsek/strcu/releases/latest/download/install.ps1))) -Uninstall
#
# Options: -Yes uninstalls without asking; -NoStart does not open StrCu's window afterwards; -From <folder>
# installs strcu.exe and strcu.exe.sha256 from a folder instead of the latest release.
#
# This file is ASCII only: Windows PowerShell reads a downloaded script as Latin-1, so Turkish letters are
# written as {c} {g} {i} {I} {o} {s} {u} ... and put in when the text is shown.

param(
    [switch]$Uninstall,
    [switch]$Yes,
    [switch]$NoStart,
    [string]$From,
    # The script starts itself again with administrator rights for the half that needs them (the service, the
    # program folder, the settings). These three are how the two halves talk; they are not for typing by hand.
    [switch]$SystemOnly,
    [string]$OldData,
    [string]$LogTo
)

# This script's own text, to hand to the administrator half. It reads the same whether the script came
# through `irm | iex` or was run as a file.
$selfSource = $MyInvocation.MyCommand.ScriptBlock.ToString()

# Run in a child scope: with `irm | iex` nothing set here stays in the user's PowerShell session
& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $repo = 'sakirsek/strcu'
    $name = 'StrCu'
    $dir = Join-Path $env:ProgramFiles 'StrCu'
    $exe = Join-Path $dir 'strcu.exe'
    $data = Join-Path $env:ProgramData 'StrCu'
    # Where 1.x kept the program and the settings, in the person's own folder
    $old = if ($OldData) { $OldData } else { Join-Path $env:LOCALAPPDATA 'strcu' }
    $oldExe = Join-Path $old 'strcu.exe'
    $startMenu = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\StrCu.lnk'
    # STRCU_DESKTOP: another folder for the desktop shortcut (tests)
    $desktopDir = if ($env:STRCU_DESKTOP) { $env:STRCU_DESKTOP } else { [Environment]::GetFolderPath('Desktop') }
    $desktop = Join-Path $desktopDir 'StrCu.lnk'
    $runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
    $rule = 'StrCu'
    # What 1.x kept and 2.0 takes over, settings first
    $kept = @('config.json', 'macros.json', 'cloudflared-token.txt', 'actions.log', 'actions.old.log', 'macro-shots')

    # StrCu's own language if it is set up, else the Windows display language
    $lang = (Get-UICulture).TwoLetterISOLanguageName
    foreach ($c in (Join-Path $data 'config.json'), (Join-Path $old 'config.json')) {
        try {
            if (Test-Path $c) {
                $saved = (Get-Content $c -Raw | ConvertFrom-Json).language
                if ($saved) { $lang = $saved; break }
            }
        } catch {}
    }
    $turkish = $lang -eq 'tr'

    function Text([string]$en, [string]$tr) {
        if (-not $turkish) { return $en }
        $letters = @(('{c}', 0xE7), ('{C}', 0xC7), ('{g}', 0x11F), ('{G}', 0x11E), ('{i}', 0x131), ('{I}', 0x130),
                     ('{o}', 0xF6), ('{O}', 0xD6), ('{s}', 0x15F), ('{S}', 0x15E), ('{u}', 0xFC), ('{U}', 0xDC))
        foreach ($l in $letters) { $tr = $tr.Replace($l[0], [string][char]$l[1]) }
        $tr
    }

    # The administrator half has a window of its own that nobody sees, so its lines go to a file as well and
    # the half the person is looking at prints them.
    function Say([string]$en, [string]$tr, [string]$color = 'Gray') {
        $line = Text $en $tr
        Write-Host $line -ForegroundColor $color
        if ($LogTo) { Add-Content -LiteralPath $LogTo -Value ($color + [char]9 + $line) -Encoding UTF8 }
    }

    function Show-Said([string]$path) {
        if (-not (Test-Path $path)) { return }
        foreach ($l in Get-Content -LiteralPath $path -Encoding UTF8) {
            $i = $l.IndexOf([char]9)
            if ($i -lt 0) { Write-Host $l } else { Write-Host $l.Substring($i + 1) -ForegroundColor $l.Substring(0, $i) }
        }
    }

    function Same-Path([string]$a, [string]$b) {
        if (-not $a -or -not $b) { return $false }
        [IO.Path]::GetFullPath($a).TrimEnd('\') -ieq [IO.Path]::GetFullPath($b).TrimEnd('\')
    }

    function Is-Admin {
        $me = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
        $me.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    }

    # Everything under a folder, whoever started it: an update replaces the program, so nothing of it may be
    # left running - the service, a worker, a window, and the cloudflared it keeps for the tunnel. A
    # cloudflared left behind would hold the tunnel open on a StrCu that is no longer there.
    function Stop-Under([string]$folder) {
        $running = @(Get-Process strcu, cloudflared -ErrorAction SilentlyContinue |
            Where-Object { $_.Path -and $_.Path.StartsWith($folder, [StringComparison]::OrdinalIgnoreCase) })
        if ($running.Count -eq 0) { return }
        $running | Stop-Process -Force -ErrorAction SilentlyContinue
        $running | Wait-Process -Timeout 15 -ErrorAction SilentlyContinue
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

    # The port the panel listens on, from the settings 2.0 keeps or the ones 1.x left; the way through the
    # firewall has to be opened for that one, since a service is never asked about it.
    function Panel-Port {
        foreach ($c in (Join-Path $data 'config.json'), (Join-Path $old 'config.json')) {
            try {
                if (Test-Path $c) {
                    $p = (Get-Content $c -Raw | ConvertFrom-Json).port
                    if ($p) { return [int]$p }
                }
            } catch {}
        }
        8765
    }

    # ---------- the half that needs administrator rights ----------

    # 1.x kept its settings in the person's own folder, which a service running as the computer cannot even
    # find: %LOCALAPPDATA% belongs to whoever runs, and that is the computer now. So the copying is done here,
    # by the installer, which knows whose computer this is.
    function Take-Over-1x {
        if (-not (Test-Path (Join-Path $old 'config.json'))) { return }
        if (Test-Path (Join-Path $data 'config.json')) { return }
        New-Item -ItemType Directory -Force $data | Out-Null
        $done = @()
        foreach ($n in $kept) {
            $f = Join-Path $old $n
            $t = Join-Path $data $n
            if ((Test-Path $f) -and -not (Test-Path $t)) {
                Copy-Item -LiteralPath $f -Destination $t -Recurse -Force
                $done += $n
            }
        }
        if ($done.Count -gt 0) {
            Say "Settings taken over from the StrCu that was here: $($done -join ', ')" `
                "Buradaki StrCu'nun ayarlar{i} devral{i}nd{i}: $($done -join ', ')" 'Green'
        }
    }

    function Install-System {
        $tmp = Join-Path ([IO.Path]::GetTempPath()) ('strcu-' + [guid]::NewGuid().ToString('N'))
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

            $service = Get-Service -Name $name -ErrorAction SilentlyContinue
            $update = Test-Path $exe
            if ($service -and $service.Status -ne 'Stopped') {
                Say 'Stopping the running StrCu...' '{C}al{i}{s}an StrCu durduruluyor...'
                Stop-Service -Name $name -Force
                (Get-Service -Name $name).WaitForStatus('Stopped', '00:00:30')
            }
            Stop-Under $dir
            New-Item -ItemType Directory -Force $dir | Out-Null
            # A program that has just stopped can stay locked for a moment
            for ($i = 0; ; $i++) {
                try { Move-Item $newExe $exe -Force; break }
                catch { if ($i -ge 20) { throw }; Start-Sleep -Milliseconds 300 }
            }

            Take-Over-1x

            if ($service) {
                # An older service may point somewhere else; left alone when it already points here
                $want = '"' + $exe + '" service'
                $have = (Get-CimInstance Win32_Service -Filter "Name='$name'").PathName
                if ($have -ne $want) { & sc.exe config $name binPath= $want start= auto | Out-Null }
            } else {
                New-Service -Name $name -BinaryPathName "`"$exe`" service" -DisplayName 'StrCu' -StartupType Automatic `
                    -Description 'StrCu: control this computer from your phone.' | Out-Null
            }
            # If StrCu itself ever stops, Windows starts it again rather than leaving the phone with nothing
            & sc.exe failure $name reset= 86400 actions= restart/5000/restart/15000/restart/60000 | Out-Null

            $port = Panel-Port
            & netsh advfirewall firewall delete rule name="$rule" | Out-Null
            & netsh advfirewall firewall add rule name="$rule" dir=in action=allow protocol=TCP localport=$port `
                profile=private program="$exe" | Out-Null

            Start-Service -Name $name
            (Get-Service -Name $name).WaitForStatus('Running', '00:00:30')
            if ($update) {
                Say "Updated: $exe" "G{u}ncellendi: $exe" 'Green'
            } else {
                Say "Installed: $exe" "Kuruldu: $exe" 'Green'
                Say "  settings: $data" "  ayarlar: $data"
                Say "  starts with Windows as a service, before anyone signs in" `
                    "  Windows ile, kimse giri{s} yapmadan {o}nce servis olarak ba{s}l{i}yor"
                Say "  the home network is open on port $port (private networks)" `
                    "  ev a{g}{i} $port portunda a{c}{i}k ({o}zel a{g}lar)"
            }
        } finally {
            Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
        }
    }

    function Uninstall-System {
        if (Get-Service -Name $name -ErrorAction SilentlyContinue) {
            Stop-Service -Name $name -Force -ErrorAction SilentlyContinue
            Start-Sleep -Seconds 1
            & sc.exe delete $name | Out-Null
        }
        Stop-Under $dir
        Stop-Under $data
        & netsh advfirewall firewall delete rule name="$rule" | Out-Null
        foreach ($d in $dir, $data) {
            if (Test-Path $d) {
                for ($i = 0; ; $i++) {
                    try { Remove-Item $d -Recurse -Force; break }
                    catch { if ($i -ge 20) { throw }; Start-Sleep -Milliseconds 300 }
                }
            }
        }
        Say 'The service, the program and the settings were removed.' `
            'Servis, program ve ayarlar kald{i}r{i}ld{i}.' 'Green'
    }

    # Starts this script again as administrator for the half above, and brings its words back.
    #
    # The copy it starts goes to the temporary folder, which is the person's own to write. A program already
    # running as that person could swap it in the moment between - but such a program can change what this
    # person runs anyway, and they are already running whatever `irm` just handed them. The name is random
    # so nothing can lie in wait for it.
    function Run-Elevated {
        $base = Join-Path ([IO.Path]::GetTempPath()) ('strcu-install-' + [guid]::NewGuid().ToString('N'))
        $script = "$base.ps1"
        $log = "$base.log"
        Set-Content -LiteralPath $script -Value $selfSource -Encoding ASCII
        $a = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $script, '-SystemOnly', '-LogTo', $log, '-OldData', $old)
        if ($Uninstall) { $a += @('-Uninstall', '-Yes') }
        if ($From) { $a += @('-From', (Resolve-Path $From).Path) }
        try {
            Say 'Windows asks for administrator approval once...' 'Windows bir kez y{o}netici onay{i} soruyor...'
            $p = Start-Process powershell -Verb RunAs -ArgumentList $a -PassThru -Wait -WindowStyle Hidden
            Show-Said $log
            return $p.ExitCode
        } catch {
            Say 'Administrator approval was not given; nothing was changed.' `
                'Y{o}netici onay{i} verilmedi; hi{c}bir {s}ey de{g}i{s}tirilmedi.' 'Red'
            return 1
        } finally {
            Remove-Item $script, $log -Force -ErrorAction SilentlyContinue
        }
    }

    # ---------- the half that is the person's own ----------

    function Install-User {
        # 1.x started with Windows from the person's own Run key. The service does that now, and the two
        # would fight over the same port, so the old entry goes.
        $run = (Get-ItemProperty $runKey -Name StrCu -ErrorAction SilentlyContinue).StrCu
        if ($run) {
            Remove-ItemProperty $runKey -Name StrCu
            Say 'The StrCu that was here no longer starts with Windows; the service does.' `
                'Buradaki StrCu art{i}k Windows ile ba{s}lam{i}yor; onu art{i}k servis yap{i}yor.'
        }
        Shortcut $startMenu
        Shortcut $desktop
        if (Test-Path $oldExe) {
            Say "Its own files are left where they are, to go back to: $old" `
                "Kendi dosyalar{i} geri d{o}nebilmek i{c}in yerinde b{i}rak{i}ld{i}: $old"
        }
    }

    function Uninstall-User {
        $run = (Get-ItemProperty $runKey -Name StrCu -ErrorAction SilentlyContinue).StrCu
        if ($run) { Remove-ItemProperty $runKey -Name StrCu }
        foreach ($lnk in $startMenu, $desktop) { if (Points-Here $lnk) { Remove-Item $lnk -Force } }
    }

    function Ask-First {
        Say 'This removes StrCu completely:' 'Bu, StrCu''yu tamamen kald{i}r{i}r:' 'Yellow'
        Say "  the StrCu service and $dir" "  StrCu servisi ve $dir"
        Say "  $data (all settings: password, paired phones, fingerprints, tunnel token)" `
            "  $data (t{u}m ayarlar: parola, e{s}le{s}mi{s} telefonlar, parmak izleri, t{u}nel jetonu)"
        Say '  the way through the firewall and the shortcuts' '  g{u}venlik duvar{i} izni ve k{i}sayollar'
        Say '  The tunnel and Access in Cloudflare stay; remove them in the dashboard if you like.' `
            '  Cloudflare''daki t{u}nel ve Access kal{i}r; istersen panelden kald{i}r.'
        if ($Yes) { return $true }
        $answer = Read-Host (Text 'Remove? [y/N]' 'Kald{i}r{i}ls{i}n m{i}? [e/H]')
        if ($answer -match '^(y|yes|e|evet)$') { return $true }
        Say 'Nothing was removed.' 'Hi{c}bir {s}ey kald{i}r{i}lmad{i}.'
        $false
    }

    # ---------- which half this is ----------

    try {
        if ($SystemOnly) {
            # Started by the half above, already elevated
            if ($Uninstall) { Uninstall-System } else { Install-System }
            exit 0
        }
        if ($Uninstall) {
            if (-not (Test-Path $dir) -and -not (Get-Service -Name $name -ErrorAction SilentlyContinue)) {
                Say 'StrCu is not installed.' 'StrCu kurulu de{g}il.'
                return
            }
            if (-not (Ask-First)) { return }
            $code = 0
            if (Is-Admin) { Uninstall-System } else { $code = Run-Elevated }
            if ($code -ne 0) { return }
            Uninstall-User
            Say 'StrCu was removed.' 'StrCu kald{i}r{i}ld{i}.' 'Green'
            return
        }

        # An older StrCu of this person's would hold the port the service wants
        Stop-Under $old
        $code = 0
        if (Is-Admin) { Install-System } else { $code = Run-Elevated }
        if ($code -ne 0) { return }
        Install-User
        Say 'Shortcuts: Start menu and desktop (StrCu).' 'K{i}sayollar: Ba{s}lat men{u}s{u} ve masa{u}st{u} (StrCu).'
        if ($NoStart) { return }
        # The window is only a window onto the service, which is already running; it is opened as the person,
        # not as an administrator, because that is whose computer it is.
        Start-Process $exe -WorkingDirectory $dir
        Say 'StrCu opened in a new window; set it up there.' 'StrCu yeni pencerede a{c}{i}ld{i}; kurulumu orada yap.'
    } catch {
        Say "Failed: $($_.Exception.Message)" "Olmad{i}: $($_.Exception.Message)" 'Red'
        if ($SystemOnly) { exit 1 }
    }
}
