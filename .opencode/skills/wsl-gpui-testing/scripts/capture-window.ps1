param(
    [string]$OutFile = "$env:TEMP\wslg-window.png",
    [string]$TitleLike = "VRC Photo Manager*",
    [string]$ProcessName = "msrdc",
    [switch]$FullScreen
)

Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class Win32Capture {
    [DllImport("user32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    public static extern IntPtr FindWindow(string cls, string title);
    [DllImport("user32.dll")]
    public static extern bool ShowWindow(IntPtr h, int cmd);
    [DllImport("user32.dll")]
    public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")]
    public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
    [DllImport("user32.dll")]
    public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")]
    public static extern bool GetClientRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")]
    public static extern bool ClientToScreen(IntPtr h, ref POINT p);
    [DllImport("user32.dll")]
    public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
    public struct RECT { public int Left, Top, Right, Bottom; }
    public struct POINT { public int X, Y; }
}
"@

$handle = [IntPtr]::Zero
$dir = Split-Path -Parent $OutFile
if ($dir -and -not (Test-Path $dir)) {
    New-Item -ItemType Directory -Path $dir | Out-Null
}

if ($FullScreen) {
    $bounds = [System.Windows.Forms.SystemInformation]::VirtualScreen
    $width = $bounds.Width
    $height = $bounds.Height
    $left = $bounds.Left
    $top = $bounds.Top
    $description = "virtual screen"
} else {
    $process = Get-Process -Name $ProcessName -ErrorAction SilentlyContinue |
        Where-Object { $_.MainWindowTitle -like $TitleLike } |
        Select-Object -First 1
    $handle = if ($process -and $process.MainWindowHandle -ne 0) {
        $process.MainWindowHandle
    } else {
        [Win32Capture]::FindWindow($null, "VRC Photo Manager (Ubuntu)")
    }

    if ($handle -eq [IntPtr]::Zero) {
        Write-Output "window not found (process=$ProcessName title=$TitleLike)"
        Get-Process |
            Where-Object { $_.MainWindowTitle -ne "" } |
            Select-Object ProcessName, MainWindowTitle |
            Format-Table -AutoSize
        exit 1
    }

    [Win32Capture]::ShowWindow($handle, 9) | Out-Null
    [Win32Capture]::SetWindowPos(
        $handle, [IntPtr](-1), 0, 0, 0, 0, 0x0001 -bor 0x0002 -bor 0x0040
    ) | Out-Null
    [Win32Capture]::SetForegroundWindow($handle) | Out-Null
    Start-Sleep -Milliseconds 1000

    $windowRect = New-Object -TypeName 'Win32Capture+RECT'
    [Win32Capture]::GetWindowRect($handle, [ref]$windowRect) | Out-Null
    $clientRect = New-Object -TypeName 'Win32Capture+RECT'
    $origin = New-Object -TypeName 'Win32Capture+POINT'
    $haveClient = [Win32Capture]::GetClientRect($handle, [ref]$clientRect)
    if ($haveClient) {
        $haveClient = [Win32Capture]::ClientToScreen($handle, [ref]$origin)
    }
    $clientWidth = $clientRect.Right - $clientRect.Left
    $clientHeight = $clientRect.Bottom - $clientRect.Top

    if ($haveClient -and $clientWidth -gt 0 -and $clientHeight -gt 0) {
        # Prefer the client area: WSLg's RAIL window rect includes an invisible
        # resize/shadow margin that would capture a strip of the desktop.
        $width = $clientWidth
        $height = $clientHeight
        $left = $origin.X
        $top = $origin.Y
        $description = "window client"
    } else {
        $width = $windowRect.Right - $windowRect.Left
        $height = $windowRect.Bottom - $windowRect.Top
        $left = $windowRect.Left
        $top = $windowRect.Top
        $description = "window"
    }

    if ($width -le 0 -or $height -le 0) {
        Write-Output "invalid window rect: ${width}x${height}"
        exit 1
    }
}

$bitmap = New-Object System.Drawing.Bitmap $width, $height
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$method = "screen"
try {
    $printed = $false
    if (-not $FullScreen -and $handle -ne [IntPtr]::Zero) {
        # PrintWindow captures only the window surface and avoids the WSLg
        # shadow/desktop strips that CopyFromScreen includes around the client
        # area. Fall back to the screen capture if the window rejects it.
        $hdc = $graphics.GetHdc()
        try {
            $printed = [Win32Capture]::PrintWindow($handle, $hdc, 2)
        } finally {
            $graphics.ReleaseHdc($hdc)
        }
        if ($printed) {
            $method = "PrintWindow"
        }
    }
    if (-not $printed) {
        $graphics.CopyFromScreen($left, $top, 0, 0, $bitmap.Size)
    }
    $bitmap.Save($OutFile, [System.Drawing.Imaging.ImageFormat]::Png)
} catch {
    Write-Output ("capture failed: {0}" -f $_.Exception.Message)
    exit 1
} finally {
    $graphics.Dispose()
    $bitmap.Dispose()
    if ($handle -and $handle -ne [IntPtr]::Zero) {
        [Win32Capture]::SetWindowPos(
            $handle, [IntPtr](-2), 0, 0, 0, 0, 0x0001 -bor 0x0002
        ) | Out-Null
    }
}

Write-Output ("path={0} type={1} method={2} size={3}x{4} at={5},{6}" -f $OutFile, $description, $method, $width, $height, $left, $top)
