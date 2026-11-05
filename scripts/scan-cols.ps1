Add-Type -AssemblyName System.Drawing
function ColRuns($path) {
    $img = [System.Drawing.Bitmap]::FromFile($path)
    Write-Output $path
    for ($x = 1820; $x -lt $img.Width; $x++) {
        $last = $null; $start = 0; $runs = @()
        for ($y = 50; $y -lt 900; $y++) {
            $c = $img.GetPixel($x, $y)
            $key = "{0},{1},{2}" -f $c.R, $c.G, $c.B
            if ($null -eq $last) { $last = $key; $start = 50 }
            elseif ($key -ne $last) {
                if (($y - $start) -ge 30) { $runs += ("{0}-{1} {2}" -f $start, ($y - 1), $last) }
                $last = $key; $start = $y
            }
        }
        if ((900 - $start) -ge 30) { $runs += ("{0}-{1} {2}" -f $start, 899, $last) }
        if ($runs.Count -gt 0) { Write-Output ("x=$x " + ($runs -join ' | ')) }
    }
    $img.Dispose()
}
ColRuns 'C:\Users\24524\AppData\Local\Temp\pi-clipboard-9ff879b0-47a9-414c-a8d1-925eb80ae459.png'
ColRuns 'C:\Users\24524\AppData\Local\Temp\pi-clipboard-6ad23fbd-d836-4482-8085-e54671573f1e.png'
