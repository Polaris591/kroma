Add-Type -AssemblyName System.Drawing
function Scan-Row($path, $y) {
    $img = [System.Drawing.Bitmap]::FromFile($path)
    Write-Output ("$path {0}x{1} row $y" -f $img.Width, $img.Height)
    $last = $null
    $start = 0
    for ($x = 0; $x -lt $img.Width; $x++) {
        $c = $img.GetPixel($x, $y)
        $key = "{0},{1},{2}" -f $c.R, $c.G, $c.B
        if ($null -eq $last) { $last = $key; $start = 0 }
        elseif ($key -ne $last) {
            if (($x - $start) -ge 4) { Write-Output ("{0}-{1} {2}" -f $start, ($x - 1), $last) }
            $last = $key
            $start = $x
        }
    }
    if (($img.Width - $start) -ge 4) { Write-Output ("{0}-{1} {2}" -f $start, ($img.Width - 1), $last) }
    $img.Dispose()
}
Scan-Row 'C:\Users\24524\AppData\Local\Temp\pi-clipboard-9ff879b0-47a9-414c-a8d1-925eb80ae459.png' 200
Scan-Row 'C:\Users\24524\AppData\Local\Temp\pi-clipboard-6ad23fbd-d836-4482-8085-e54671573f1e.png' 200
