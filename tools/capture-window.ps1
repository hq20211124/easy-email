# Capture the Easy Email window to a PNG so the UI can be inspected.
#
# Finds the window by title rather than trusting `Process.MainWindowHandle` (WebView2 spawns
# several top-level windows, so the "main" handle can point at a helper). Brings the window to
# the foreground before capturing: BitBlt from the screen is more reliable than PrintWindow for
# a WebView2 surface.
Add-Type -AssemblyName System.Drawing

Add-Type @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public static class Win {
    public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);

    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr lParam);
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int cmd);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT r);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);

    /// Make this process DPI-aware.
    ///
    /// Without it every coordinate below is virtualised: on a 150%-scaled display a DPI-unaware
    /// process sees the desktop as 1440x960 while the window is really 1921 physical pixels wide,
    /// so `GetWindowRect` reports ~1295 and `CopyFromScreen` captures only the LEFT THIRD of the
    /// window. That silently cropped the reader pane out of every screenshot and is what made the
    /// layout look broken when it was not.
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();

    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left, Top, Right, Bottom; }

    static readonly IntPtr HWND_TOPMOST = new IntPtr(-1);
    static readonly IntPtr HWND_NOTOPMOST = new IntPtr(-2);
    const uint SWP_NOMOVE = 0x0002, SWP_NOSIZE = 0x0001, SWP_SHOWWINDOW = 0x0040;

    /// Raise a window above everything else, and KEEP it there.
    ///
    /// SetForegroundWindow alone is not enough: a background process is usually denied the
    /// foreground, and the capture then photographs whatever else happens to be at those screen
    /// coordinates. A topmost toggle is what makes it work — but it has to stay topmost until the
    /// bitmap has been taken. The first version of this released topmost before capturing, and the
    /// screenshot came back showing the wrong window.
    public static void MakeTopmost(IntPtr h) {
        ShowWindow(h, 9);   // SW_RESTORE
        ShowWindow(h, 5);   // SW_SHOW
        SetWindowPos(h, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW);
        SetForegroundWindow(h);
    }

    /// Put the window back into the normal z-order once the capture is done.
    public static void ReleaseTopmost(IntPtr h) {
        SetWindowPos(h, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW);
    }

    /// Largest visible window whose title contains `needle`, for the given process id.
    public static IntPtr FindMain(uint targetPid, string needle) {
        IntPtr best = IntPtr.Zero;
        long bestArea = 0;
        EnumWindows((h, p) => {
            uint pid;
            GetWindowThreadProcessId(h, out pid);
            if (pid != targetPid || !IsWindowVisible(h)) return true;

            var title = new StringBuilder(512);
            GetWindowText(h, title, 512);
            if (needle.Length > 0 && !title.ToString().Contains(needle)) return true;

            RECT r;
            if (!GetWindowRect(h, out r)) return true;
            long area = (long)(r.Right - r.Left) * (r.Bottom - r.Top);
            if (area > bestArea) { bestArea = area; best = h; }
            return true;
        }, IntPtr.Zero);
        return best;
    }
}
'@

$proc = Get-Process -Name 'easy-email' -ErrorAction Stop | Select-Object -First 1

# Must run before any window or GDI call below; see the note on Win.SetProcessDPIAware.
[void][Win]::SetProcessDPIAware()

$hwnd = [Win]::FindMain([uint32]$proc.Id, 'Easy Email')
if ($hwnd -eq [IntPtr]::Zero) { throw "no visible Easy Email window for pid $($proc.Id)" }

[Win]::MakeTopmost($hwnd)
Start-Sleep -Milliseconds 1500

$rect = New-Object Win+RECT
[Win]::GetWindowRect($hwnd, [ref]$rect) | Out-Null
$width = $rect.Right - $rect.Left
$height = $rect.Bottom - $rect.Top

if ($width -lt 400 -or $height -lt 300) {
    [Win]::ReleaseTopmost($hwnd)
    throw "window rect looks wrong: ${width}x${height} (${hwnd})"
}

$bitmap = New-Object System.Drawing.Bitmap($width, $height)
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$graphics.CopyFromScreen($rect.Left, $rect.Top, 0, 0, $bitmap.Size)

# Only now is it safe to drop back into the normal z-order.
[Win]::ReleaseTopmost($hwnd)

$out = Join-Path (Get-Location) 'target\screenshot.png'
$bitmap.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)
$graphics.Dispose()
$bitmap.Dispose()

"captured ${width}x${height} -> $out"
