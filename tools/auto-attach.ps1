# Auto-attach loop for the virtual QA40x (usbip-win2).
#
# Run this BEFORE triggering a firmware update in the host app: the update
# makes the device "reboot" twice (analyzer -> NXP bootloader -> analyzer) and
# each reboot detaches the USB/IP device; this loop re-attaches it within
# ~IntervalMs so the app's bootloader-wait timeout is met, like the real
# hardware's fast re-enumeration.
#
# While the device is attached the attach attempt fails silently — that is
# expected and harmless.
#
# Usage:  powershell -ExecutionPolicy Bypass -File auto-attach.ps1 [-Server 10.211.55.2]

param(
    [string]$Server = "10.211.55.2",
    [string]$BusId = "1-1",
    [string]$UsbIp = "C:\Program Files\USBip\usbip.exe",
    [int]$IntervalMs = 300
)

Write-Host "Auto-attaching $BusId from ${Server} every ${IntervalMs} ms (Ctrl-C to stop)"
$attached = $false
while ($true) {
    & $UsbIp attach -r $Server -b $BusId *> $null
    if ($LASTEXITCODE -eq 0) {
        Write-Host ("[{0:HH:mm:ss}] attached" -f (Get-Date))
        $attached = $true
    } elseif ($attached) {
        # Informative only: distinguishes "still attached" from "gone".
        $attached = $true
    }
    Start-Sleep -Milliseconds $IntervalMs
}
