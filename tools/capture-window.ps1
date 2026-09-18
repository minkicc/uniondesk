# Captures a single window's own surface into a PNG.
#
# Uses PrintWindow, so it renders the target window even when it is occluded and
# never captures anything else on the desktop. Handy for documentation
# screenshots without exposing whatever else is on screen.
#
# Usage: powershell -ExecutionPolicy Bypass -File tools/capture-window.ps1 -Title UnionDesk -Out shot.png

param(
    [Parameter(Mandatory = $true)][string]$Title,
    [Parameter(Mandatory = $true)][string]$Out
)

Add-Type -AssemblyName System.Drawing

$signature = @"
using System;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;

public class WindowCapture {
    public delegate bool EnumProc(IntPtr hwnd, IntPtr lParam);

    [DllImport("user32.dll")]
    public static extern bool EnumWindows(EnumProc callback, IntPtr lParam);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern int GetWindowTextW(IntPtr hwnd, System.Text.StringBuilder text, int max);

    [DllImport("user32.dll")]
    public static extern bool IsWindowVisible(IntPtr hwnd);

    [DllImport("user32.dll")]
    public static extern bool SetProcessDPIAware();

    [DllImport("user32.dll")]
    public static extern bool PrintWindow(IntPtr hwnd, IntPtr hdc, uint flags);

    [DllImport("user32.dll")]
    public static extern bool GetWindowRect(IntPtr hwnd, out RECT rect);

    [DllImport("user32.dll")]
    public static extern bool GetClientRect(IntPtr hwnd, out RECT rect);

    [DllImport("user32.dll")]
    public static extern bool ClientToScreen(IntPtr hwnd, ref POINT point);

    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left, Top, Right, Bottom; }

    [StructLayout(LayoutKind.Sequential)]
    public struct POINT { public int X, Y; }

    public static string Capture(string title, string path) {
        // Without this the process sees virtualised (logical) coordinates while
        // the WebView2 surface is rendered at physical pixel size, so the
        // captured image would be a cropped corner of the real window.
        SetProcessDPIAware();
        // A tray icon also creates a hidden window carrying the application
        // title, so pick the largest visible one instead of the first match.
        IntPtr hwnd = IntPtr.Zero;
        long best = 0;
        var text = new System.Text.StringBuilder(512);
        EnumWindows(delegate(IntPtr candidate, IntPtr _) {
            if (!IsWindowVisible(candidate)) return true;
            text.Clear();
            GetWindowTextW(candidate, text, text.Capacity);
            if (text.ToString() != title) return true;
            RECT bounds;
            if (!GetWindowRect(candidate, out bounds)) return true;
            long area = (long)(bounds.Right - bounds.Left) * (bounds.Bottom - bounds.Top);
            if (area > best) {
                best = area;
                hwnd = candidate;
            }
            return true;
        }, IntPtr.Zero);
        if (hwnd == IntPtr.Zero) return "window not found: " + title;

        RECT rect;
        if (!GetClientRect(hwnd, out rect)) return "could not read the window bounds";
        int width = rect.Right - rect.Left;
        int height = rect.Bottom - rect.Top;
        if (width <= 0 || height <= 0) return "window has no area";

        using (var bitmap = new Bitmap(width, height, PixelFormat.Format32bppArgb)) {
            using (var graphics = Graphics.FromImage(bitmap)) {
                IntPtr hdc = graphics.GetHdc();
                // 1 = client area only, 2 = render full content (needed for the
                // composited WebView2 surface).
                PrintWindow(hwnd, hdc, 1 | 2);
                graphics.ReleaseHdc(hdc);
            }
            bitmap.Save(path, ImageFormat.Png);
        }
        return "saved " + width + "x" + height + " to " + path;
    }
}
"@

Add-Type -TypeDefinition $signature -ReferencedAssemblies System.Drawing
Write-Output ([WindowCapture]::Capture($Title, (Resolve-Path -LiteralPath (Split-Path -Parent $Out)).Path + "\" + (Split-Path -Leaf $Out)))
