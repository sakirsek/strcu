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
# Uninstall in Settings -> Apps -> Installed apps, and in StrCu's own settings, does the same with a copy of this
# script kept beside the program.
#
# Options: -Yes uninstalls without asking; -NoStart does not open StrCu's window afterwards; -From <folder>
# installs strcu.exe and strcu.exe.sha256 from a folder instead of the latest release; -Pause waits for Enter
# before the window closes.
#
# This file is ASCII only: Windows PowerShell reads a downloaded script as Latin-1, so Turkish letters are
# written as {c} {g} {i} {I} {o} {s} {u} ... and put in when the text is shown.

param(
    [switch]$Uninstall,
    [switch]$Yes,
    [switch]$NoStart,
    [string]$From,
    [switch]$Pause,
    # The script starts itself again with administrator rights for the half that needs them (the service, the
    # program folder, the settings). These three are how the two halves talk; they are not for typing by hand.
    [switch]$SystemOnly,
    [string]$OldData,
    [string]$LogTo
)

# This script's own text, to hand to the administrator half. A script run as a file can read itself. One
# that came through `irm | iex` has no file to read, and what PowerShell tells such a script about itself is
# the text of whoever called it - not this script at all - so Run-Elevated fetches it again instead, from
# the same place it came from, and asks for approval only once it holds the whole thing.
$selfSource = if ($PSCommandPath) { [IO.File]::ReadAllText($PSCommandPath) } else { '' }

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
    # What Settings -> Apps -> Installed apps lists
    $appKey = "HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\$name"
    $rule = 'StrCu'
    # What 1.x kept and 2.0 takes over, settings first. The tunnel program itself is not among them: it runs
    # with the computer's rights, and one that sat in the person's own folder could have been changed by
    # anything they ran. StrCu fetches the one it checks for by itself, before the tunnel starts, so remote
    # access still comes back on its own.
    $kept = @('config.json', 'macros.json', 'cloudflared-token.txt', 'actions.log', 'actions.old.log', 'macro-shots')

    # The language a settings file names: '' when it leaves that to Windows, $null when it cannot be read.
    function Config-Language([string]$file) {
        try {
            if (Test-Path -LiteralPath $file) {
                return [string](Get-Content -LiteralPath $file -Raw | ConvertFrom-Json).language
            }
        } catch {}
        $null
    }

    # `count` bytes from StrCu's pipe, waiting two seconds at most for each part.
    function Read-Pipe($pipe, [int]$count) {
        $buf = New-Object byte[] $count
        $got = 0
        while ($got -lt $count) {
            $read = $pipe.ReadAsync($buf, $got, $count - $got)
            if (-not $read.Wait(2000) -or $read.Result -le 0) { throw 'StrCu did not answer' }
            $got += $read.Result
        }
        , $buf
    }

    # One ask on StrCu's pipe: four bytes of length, then JSON, each way (frame.rs).
    function Ask-Pipe($pipe, [string]$json) {
        $body = [Text.Encoding]::UTF8.GetBytes($json)
        [byte[]]$frame = [BitConverter]::GetBytes([uint32]$body.Length) + $body
        if (-not $pipe.WriteAsync($frame, 0, $frame.Length).Wait(2000)) { throw 'StrCu did not listen' }
        $len = [BitConverter]::ToUInt32((Read-Pipe $pipe 4), 0)
        if ($len -gt 1MB) { throw 'the answer is too long' }
        [Text.Encoding]::UTF8.GetString((Read-Pipe $pipe ([int]$len))) | ConvertFrom-Json
    }

    # The running StrCu's language, as the window would ask it (`Settings` needs no password). A StrCu that
    # speaks another protocol version says which, and is asked again in that one.
    function Pipe-Language {
        if (-not (Test-Path -LiteralPath "\\.\pipe\$name")) { return $null }
        $pipe = New-Object IO.Pipes.NamedPipeClientStream('.', $name, [IO.Pipes.PipeDirection]::InOut,
            [IO.Pipes.PipeOptions]::Asynchronous)
        try {
            $pipe.Connect(2000)
            # ipc.rs VERSION
            $v = 6
            foreach ($try in 1, 2) {
                $reply = Ask-Pipe $pipe ('{"v":' + $v + ',"ask":"settings"}')
                if ($reply.answer -eq 'settings') { return [string]$reply.with.language }
                if ($reply.answer -ne 'wrong_version') { break }
                $v = [int]$reply.with.want
            }
        } catch {
        } finally {
            $pipe.Dispose()
        }
        $null
    }

    # StrCu's own language, else the Windows display language. Only administrators may read the settings, so
    # the person's half asks the running StrCu; what 1.x kept counts only when neither answers.
    $lang = Config-Language (Join-Path $data 'config.json')
    if ($null -eq $lang) { $lang = Pipe-Language }
    if ($null -eq $lang) { $lang = Config-Language (Join-Path $old 'config.json') }
    if (-not $lang) { $lang = (Get-UICulture).TwoLetterISOLanguageName }
    $turkish = $lang -eq 'tr'

    function Letters([string]$s) {
        $letters = @(('{c}', 0xE7), ('{C}', 0xC7), ('{g}', 0x11F), ('{G}', 0x11E), ('{i}', 0x131), ('{I}', 0x130),
                     ('{o}', 0xF6), ('{O}', 0xD6), ('{s}', 0x15F), ('{S}', 0x15E), ('{u}', 0xFC), ('{U}', 0xDC))
        foreach ($l in $letters) { $s = $s.Replace($l[0], [string][char]$l[1]) }
        $s
    }

    function Text([string]$en, [string]$tr) {
        if ($turkish) { Letters $tr } else { $en }
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
        # Inside the folder itself, not every folder whose name starts the same way (StrCu and StrCuDev)
        $inside = $folder.TrimEnd('\') + '\'
        $running = @(Get-Process strcu, cloudflared -ErrorAction SilentlyContinue |
            Where-Object { $_.Path -and $_.Path.StartsWith($inside, [StringComparison]::OrdinalIgnoreCase) })
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

    # The settings folder, before a single setting goes in it. StrCu closes it itself every time the service
    # starts, but the first start comes after this: anybody may create a folder in %ProgramData%, and whoever
    # creates one owns it, so a folder that is already there under somebody else's name is moved out of the
    # way rather than used - it would hold the password, the registered phones and the tunnel token.
    # The settings belong to the administrators and the system, and to nobody else. A folder made in
    # %ProgramData% starts out letting any user write in it, so this follows the making at once. StrCu does
    # the same itself, every time the service starts, and over the whole folder (paths.rs).
    function Close-Data {
        & icacls.exe $data /setowner '*S-1-5-32-544' /q | Out-Null
        & icacls.exe $data /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' /q | Out-Null
    }

    # `update` says StrCu was already installed here. Only an administrator can put a program in
    # %ProgramFiles%, so its settings are its own however they came to be owned, and they are taken over
    # where they are. Without that sign the folder is somebody's doing and is moved out of the way: settings
    # left for us to find could name a password and a phone that are not ours.
    function Guard-Data([bool]$update) {
        if ((Test-Path $data) -and -not (Only-Admins-Change $data)) {
            if ($update) {
                Say 'The settings were not the administrators'' alone; they are now.' `
                    'Ayarlar yaln{i}zca y{o}neticilerin de{g}ildi; art{i}k {o}yle.' 'Yellow'
            } else {
                $aside = "$data.untrusted-" + (Get-Date).ToString('yyyyMMdd-HHmmss')
                Move-Item -LiteralPath $data -Destination $aside -Force
                Say "The settings folder belonged to somebody else; it was moved aside as $aside and a new one made." `
                    "Ayarlar klas{o}r{u} ba{s}kas{i}na aitti; $aside olarak kenara al{i}nd{i} ve yenisi yap{i}ld{i}." 'Yellow'
            }
        }
        New-Item -ItemType Directory -Force $data | Out-Null
        Close-Data
        if (-not (Only-Admins-Change $data)) {
            throw (Text "$data can be changed by an account that is not an administrator." `
                "$data y{o}netici olmayan bir hesap taraf{i}ndan de{g}i{s}tirilebiliyor.")
        }
    }

    # 1.x kept its settings in the person's own folder, which a service running as the computer cannot even
    # find: %LOCALAPPDATA% belongs to whoever runs, and that is the computer now. So the copying is done here,
    # by the installer, which knows whose computer this is.
    function Take-Over-1x {
        if (-not (Test-Path (Join-Path $old 'config.json'))) { return }
        if (Test-Path (Join-Path $data 'config.json')) { return }
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

    # .NET directly: Get-FileHash is missing when Windows PowerShell inherits PowerShell 7's module path
    function Sha256([string]$path) {
        $sha = [Security.Cryptography.SHA256]::Create()
        $stream = [IO.File]::OpenRead($path)
        try { -join ($sha.ComputeHash($stream) | ForEach-Object { $_.ToString('x2') }) } finally { $stream.Dispose() }
    }

    # Can only the system, the administrators and TrustedInstaller change this file or folder? The service runs
    # the program with the computer's own rights, so whoever else can change it can have those rights too.
    # .NET directly again, for the same reason as above; this works in both PowerShells.
    function Only-Admins-Change([string]$path) {
        $trusted = @('S-1-5-18', 'S-1-5-32-544', 'S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464')
        # Write, append, write attributes, delete a child, delete, change the rules, take ownership, generic write/all
        $change = 0x500D0156
        $sections = [Security.AccessControl.AccessControlSections]'Owner, Access'
        $acl = if (Test-Path -LiteralPath $path -PathType Container) {
            New-Object Security.AccessControl.DirectorySecurity($path, $sections)
        } else {
            New-Object Security.AccessControl.FileSecurity($path, $sections)
        }
        $sid = [Security.Principal.SecurityIdentifier]
        if ($trusted -notcontains $acl.GetOwner($sid).Value) { return $false }
        foreach ($r in $acl.GetAccessRules($true, $true, $sid)) {
            if ($r.AccessControlType -ne 'Allow') { continue }
            if ($r.PropagationFlags.HasFlag([Security.AccessControl.PropagationFlags]::InheritOnly)) { continue }
            if ($trusted -contains $r.IdentityReference.Value) { continue }
            if (([int]$r.FileSystemRights -band $change) -ne 0) { return $false }
        }
        $true
    }

    # The copy of this script that Uninstall in Installed apps and in StrCu's window runs: nothing to download
    # then, and like the program beside it, only administrators can change it. $null when there is none.
    function Keep-Installer {
        $copy = Join-Path $dir 'install.ps1'
        $new = Join-Path $dir 'install.new.ps1'
        try {
            if (Is-Whole $selfSource) {
                [IO.File]::WriteAllText($new, $selfSource, [Text.Encoding]::ASCII)
            } else {
                # An administrator who ran `irm | iex`: this script has no file of its own (see Run-Elevated)
                Invoke-WebRequest "https://github.com/$repo/releases/latest/download/install.ps1" -UseBasicParsing -OutFile $new
            }
            & icacls.exe $new /setowner '*S-1-5-32-544' /q | Out-Null
            if ((Is-Whole ([IO.File]::ReadAllText($new))) -and (Only-Admins-Change $new)) {
                Move-Item -LiteralPath $new -Destination $copy -Force
                return $copy
            }
        } catch {}
        Remove-Item -LiteralPath $new -Force -ErrorAction SilentlyContinue
        $null
    }

    # StrCu in Settings -> Apps -> Installed apps, with its icon, version and an Uninstall that runs the copy above.
    # The window the copy opens waits for Enter, so what it did can be read; tools such as winget use the quiet one.
    function Register-App([string]$version, [string]$copy) {
        $ps = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
        $run = "`"$ps`" -NoProfile -ExecutionPolicy Bypass -File `"$copy`" -Uninstall"
        $size = (Get-ChildItem -LiteralPath $dir -Recurse -File | Measure-Object Length -Sum).Sum
        New-Item -Path $appKey -Force | Out-Null
        $texts = [ordered]@{
            DisplayName = $name
            DisplayVersion = $version
            DisplayIcon = $exe
            Publisher = Letters '{S}akir {S}ek'
            URLInfoAbout = "https://github.com/$repo"
            InstallLocation = $dir
            InstallDate = (Get-Date).ToString('yyyyMMdd')
            UninstallString = "$run -Pause"
            QuietUninstallString = "$run -Yes"
        }
        foreach ($k in $texts.Keys) { New-ItemProperty $appKey -Name $k -Value $texts[$k] -PropertyType String -Force | Out-Null }
        $numbers = [ordered]@{ NoModify = 1; NoRepair = 1; EstimatedSize = [int]($size / 1KB) }
        foreach ($k in $numbers.Keys) { New-ItemProperty $appKey -Name $k -Value $numbers[$k] -PropertyType DWord -Force | Out-Null }
    }

    function Install-System {
        $tmp = Join-Path ([IO.Path]::GetTempPath()) ('strcu-' + [guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory $tmp | Out-Null
        # What runs is a copy in the program folder, checked there. The temporary folder is the person's own, and a
        # file moved out of it keeps the rules it had there, the person's right to change it among them; a copy is
        # a new file and takes the rules of the folder it is made in, where only administrators may write.
        $staged = Join-Path $dir 'strcu.new.exe'
        # The running StrCu stopped and its program not replaced yet: anything that fails in between starts it
        # again as it was, so a failed update does not leave the computer without StrCu
        $stopped = $false
        $replaced = $false
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
            $mismatch = Text 'The download does not match its SHA-256 checksum; nothing was changed.' `
                '{I}ndirilen dosya SHA-256 {o}zetiyle tutmuyor; hi{c}bir {s}ey de{g}i{s}tirilmedi.'
            # Once here, so a bad download changes nothing, and again on the copy that is going to run
            if ((Sha256 $newExe) -ine $want) { throw $mismatch }
            New-Item -ItemType Directory -Force $dir | Out-Null
            Copy-Item -LiteralPath $newExe -Destination $staged -Force
            if ((Sha256 $staged) -ine $want) { throw $mismatch }
            $version = (& $staged --version) -replace '^strcu\s+', ''
            Say "StrCu $version, SHA-256 verified." "StrCu $version, SHA-256 do{g}ruland{i}." 'Green'

            $service = Get-Service -Name $name -ErrorAction SilentlyContinue
            $update = Test-Path $exe
            if ($service -and $service.Status -ne 'Stopped') {
                Say 'Stopping the running StrCu...' '{C}al{i}{s}an StrCu durduruluyor...'
                $stopped = $true
                Stop-Service -Name $name -Force
                (Get-Service -Name $name).WaitForStatus('Stopped', '00:00:30')
            }
            Stop-Under $dir
            # A program that has just stopped can stay locked for a moment. Within one folder a move keeps the rules
            # the copy was given there.
            for ($i = 0; ; $i++) {
                try { Move-Item -LiteralPath $staged -Destination $exe -Force; break }
                catch { if ($i -ge 20) { throw }; Start-Sleep -Milliseconds 300 }
            }
            $replaced = $true
            # Both belong to the administrators, whoever Windows makes new files belong to, and the program takes the
            # folder's rules: an install by 2.0.3 or older left it with rules of its own
            foreach ($p in $dir, $exe) { & icacls.exe $p /setowner '*S-1-5-32-544' /q | Out-Null }
            & icacls.exe $exe /reset /q | Out-Null
            foreach ($p in $dir, $exe) {
                if (-not (Only-Admins-Change $p)) {
                    throw (Text "$p can be changed by an account that is not an administrator, so StrCu was not started." `
                        "$p y{o}netici olmayan bir hesap taraf{i}ndan de{g}i{s}tirilebiliyor; bu y{u}zden StrCu ba{s}lat{i}lmad{i}.")
                }
            }

            Guard-Data $update
            Take-Over-1x

            if ($service) {
                # An older service may point somewhere else; left alone when it already points here
                $want = '"' + $exe + '" service'
                $have = (Get-CimInstance Win32_Service -Filter "Name='$name'").PathName
                if ($have -ne $want) { & sc.exe config $name binPath= $want start= auto | Out-Null }
            } else {
                New-Service -Name $name -BinaryPathName "`"$exe`" service" -DisplayName 'StrCu' -StartupType Automatic | Out-Null
            }
            # Set on updates too, so older installs read the same
            & sc.exe description $name 'StrCu: control this computer from a phone, a tablet or another computer.' | Out-Null
            # If StrCu itself ever stops, Windows starts it again rather than leaving the phone with nothing
            & sc.exe failure $name reset= 86400 actions= restart/5000/restart/15000/restart/60000 | Out-Null

            $port = Panel-Port
            & netsh advfirewall firewall delete rule name="$rule" | Out-Null
            & netsh advfirewall firewall add rule name="$rule" dir=in action=allow protocol=TCP localport=$port `
                profile=private program="$exe" | Out-Null

            Start-Service -Name $name
            (Get-Service -Name $name).WaitForStatus('Running', '00:00:30')
            # Not worth failing an install over: the command in the README uninstalls all the same
            $copy = Keep-Installer
            try {
                if (-not $copy) { throw 'no copy' }
                Register-App $version $copy
            } catch {
                Say 'StrCu could not be added to Installed apps; the command in the README uninstalls it.' `
                    'StrCu Y{u}kl{u} uygulamalar''a eklenemedi; README''deki komutla kald{i}r{i}labilir.' 'Yellow'
            }
            if ($update) {
                Say "Updated: $exe" "G{u}ncellendi: $exe" 'Green'
            } else {
                Say "Installed: $exe" "Kuruldu: $exe" 'Green'
                Say "  settings: $data" "  ayarlar: $data"
                Say "  starts with Windows as a service, before anyone signs in" `
                    "  Windows ile, kimse giri{s} yapmadan {o}nce servis olarak ba{s}l{i}yor"
                Say "  the home network is open on port $port (private networks)" `
                    "  ev a{g}{i} $port portunda a{c}{i}k ({o}zel a{g}lar)"
                Say '  uninstall: Settings -> Apps -> Installed apps, or StrCu''s own settings' `
                    '  kald{i}rmak i{c}in: Ayarlar -> Uygulamalar -> Y{u}kl{u} uygulamalar ya da StrCu''nun ayarlar{i}'
            }
        } finally {
            if ($stopped -and -not $replaced) { Start-Service -Name $name -ErrorAction SilentlyContinue }
            # Left only when something failed before the copy took the program's place
            Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue
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
        Remove-Item -Path $appKey -Recurse -Force -ErrorAction SilentlyContinue
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

    # Is this the whole script, or a piece of something else? The mark is the very last line of the file, and
    # it is written in two halves here so that the only whole one in the file is that last line itself.
    function Is-Whole($text) {
        $text -and $text.Length -gt 15000 -and $text -match 'function Install-System' -and $text -match ('STRCU-INSTALLER' + '-END')
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

        # The file that is about to run as administrator is the one that gets checked, not a copy of it in
        # memory. A piped script has itself fetched from the release straight into that file: GitHub hands
        # install.ps1 over as a file of bytes, which `Invoke-WebRequest` would give us as bytes rather than
        # text, and a check on bytes passes nothing.
        if (Is-Whole $selfSource) {
            Set-Content -LiteralPath $script -Value $selfSource -Encoding ASCII
        } else {
            Invoke-WebRequest "https://github.com/$repo/releases/latest/download/install.ps1" -UseBasicParsing -OutFile $script
        }
        # Whole, or not at all: a script with its end cut off would ask for administrator approval and then
        # quietly do nothing, which is worse than not starting at all.
        if (-not (Is-Whole ([IO.File]::ReadAllText($script)))) {
            Remove-Item $script -Force -ErrorAction SilentlyContinue
            Say 'This installer could not read itself, so it is not asking for administrator approval.' `
                'Bu kurucu kendi metnini okuyamad{i}; y{o}netici onay{i} istemiyor.' 'Red'
            Say "Download install.ps1 from github.com/$repo/releases and run it as a file." `
                "install.ps1'i github.com/$repo/releases adresinden indirip dosya olarak {c}al{i}{s}t{i}r."
            return 1
        }
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
        # Windows keeps the icons it has drawn: shortcuts from before StrCu had one would stay blank
        try { & (Join-Path $env:SystemRoot 'System32\ie4uinit.exe') -show } catch {}
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
            if (Get-Service -Name $name -ErrorAction SilentlyContinue) {
                Say 'StrCu is still installed, so nothing of yours was changed.' `
                    'StrCu h{a}l{a} kurulu; senin taraf{i}nda hi{c}bir {s}ey de{g}i{s}tirilmedi.' 'Red'
                return
            }
            Uninstall-User
            Say 'StrCu was removed.' 'StrCu kald{i}r{i}ld{i}.' 'Green'
            return
        }

        # An older StrCu of this person's would hold the port the service wants
        Stop-Under $old
        $code = 0
        if (Is-Admin) { Install-System } else { $code = Run-Elevated }
        if ($code -ne 0) { return }
        # What is on disk is the honest answer, not what the half above returned: the person's own side - the
        # entry that starts 1.x with Windows, the shortcuts - is only changed once StrCu is really there.
        if (-not (Test-Path $exe) -or -not (Get-Service -Name $name -ErrorAction SilentlyContinue)) {
            Say 'StrCu was not installed, so nothing of yours was changed.' `
                'StrCu kurulmad{i}; senin taraf{i}nda hi{c}bir {s}ey de{g}i{s}tirilmedi.' 'Red'
            return
        }
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
    } finally {
        # Started from Installed apps or StrCu's window, whose new window closes as soon as this ends
        if ($Pause -and -not $SystemOnly) { [void](Read-Host (Text 'Press Enter to close' 'Kapatmak i{c}in Enter''a bas')) }
    }
}

# The last line of this file, and the mark the installer looks for before it hands itself to the
# administrator half: whole, or it does not ask for approval at all.
# STRCU-INSTALLER-END
