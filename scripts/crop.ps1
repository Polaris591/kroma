Add-Type -AssemblyName System.Drawing
$src = [System.Drawing.Image]::FromFile('C:\Users\24524\AppData\Local\Temp\pi-clipboard-f6acb0e3-521f-4ec8-b007-68f5ade2be31.png')
$w = [int]350
$h = [int]330
$x = [int]($src.Width - $w)
$y = [int]580
$bmp = New-Object System.Drawing.Bitmap ([int]($w * 2)), ([int]($h * 2))
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::NearestNeighbor
$g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::Half
$dst = New-Object System.Drawing.Rectangle 0, 0, ([int]($w * 2)), ([int]($h * 2))
$srcRect = New-Object System.Drawing.Rectangle $x, $y, $w, $h
$g.DrawImage($src, $dst, $srcRect, [System.Drawing.GraphicsUnit]::Pixel)
$bmp.Save('D:\Kroma\kroma-latest\reviews\tui-plain-runtime\crop-right.png')
$src.Dispose()
$bmp.Dispose()
