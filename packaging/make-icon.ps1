<#
.SYNOPSIS
  生成 wx-tool 的应用图标。

.DESCRIPTION
  产出：
    assets\icon.ico      多尺寸图标（16/20/24/32/48/64/128/256），用于 exe 资源、
                         快捷方式、以及「设置 → 应用」里的显示
    assets\icon-256.png  256×256 PNG，程序运行时用作窗口/任务栏图标

  设计说明：
    两个叠放的圆角方块 —— 通用的「副本 / 多开」意象。
    刻意不使用微信的双气泡图形，那是腾讯的注册商标，仿了既容易混淆也不合适。
    底色用祖母绿渐变，和微信的绿色系呼应但图形完全不同。

  .ico 内部各尺寸都以 PNG 方式存储（Vista 以上均支持），这样 256×256 也不会太大。

.EXAMPLE
  pwsh packaging\make-icon.ps1
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
Add-Type -AssemblyName System.Drawing

$ProjectDir = Split-Path -Parent $PSScriptRoot
$AssetsDir = Join-Path $ProjectDir 'assets'
New-Item -ItemType Directory -Force $AssetsDir | Out-Null

# 圆角矩形路径
function New-RoundedPath {
    param([float]$X, [float]$Y, [float]$W, [float]$H, [float]$R)
    $p = New-Object System.Drawing.Drawing2D.GraphicsPath
    $d = $R * 2
    $p.AddArc($X, $Y, $d, $d, 180, 90)
    $p.AddArc($X + $W - $d, $Y, $d, $d, 270, 90)
    $p.AddArc($X + $W - $d, $Y + $H - $d, $d, $d, 0, 90)
    $p.AddArc($X, $Y + $H - $d, $d, $d, 90, 90)
    $p.CloseFigure()
    return $p
}

function New-IconBitmap {
    param([int]$Size)

    $s = [float]$Size
    $bmp = New-Object System.Drawing.Bitmap($Size, $Size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality

    # ---- 背景：祖母绿渐变圆角方块 ----
    $bgPath = New-RoundedPath -X 0 -Y 0 -W $s -H $s -R ($s * 0.22)
    $brush = New-Object System.Drawing.Drawing2D.LinearGradientBrush(
        (New-Object System.Drawing.Point(0, 0)),
        (New-Object System.Drawing.Point($Size, $Size)),
        [System.Drawing.Color]::FromArgb(255, 16, 185, 129),   # #10B981
        [System.Drawing.Color]::FromArgb(255, 4, 120, 87))     # #047857
    $g.FillPath($brush, $bgPath)
    $brush.Dispose()
    $bgPath.Dispose()

    # 两个方块的几何：后面一个偏左上，前面一个偏右下
    $side = $s * 0.40
    $rad = $s * 0.10
    $backX = $s * 0.19; $backY = $s * 0.19
    $frontX = $s * 0.41; $frontY = $s * 0.41

    # ---- 后面的方块：半透明白，只是个"影子"，暗示还有一份 ----
    $backPath = New-RoundedPath -X $backX -Y $backY -W $side -H $side -R $rad
    $backBrush = New-Object System.Drawing.SolidBrush(
        [System.Drawing.Color]::FromArgb(140, 255, 255, 255))
    $g.FillPath($backBrush, $backPath)
    $backBrush.Dispose()
    $backPath.Dispose()

    # ---- 用背景色"挖"出一圈间隙，让前后两个方块分开 ----
    $gap = [Math]::Max(1.0, $s * 0.055)
    $gapPath = New-RoundedPath -X ($frontX - $gap) -Y ($frontY - $gap) `
        -W ($side + $gap * 2) -H ($side + $gap * 2) -R ($rad + $gap * 0.6)
    $gapBrush = New-Object System.Drawing.SolidBrush(
        [System.Drawing.Color]::FromArgb(255, 6, 148, 106))
    $g.FillPath($gapBrush, $gapPath)
    $gapBrush.Dispose()
    $gapPath.Dispose()

    # ---- 前面的方块：实心白 ----
    $frontPath = New-RoundedPath -X $frontX -Y $frontY -W $side -H $side -R $rad
    $frontBrush = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::White)
    $g.FillPath($frontBrush, $frontPath)
    $frontBrush.Dispose()
    $frontPath.Dispose()

    $g.Dispose()
    return $bmp
}

function Get-PngBytes {
    param([System.Drawing.Bitmap]$Bitmap)
    $ms = New-Object System.IO.MemoryStream
    $Bitmap.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $bytes = $ms.ToArray()
    $ms.Dispose()
    return , $bytes
}

# 传统 BMP(DIB) 格式的图标数据：BITMAPINFOHEADER + 倒序的 BGRA 像素 + AND 掩码。
#
# 为什么不全用 PNG：Windows Installer 从 MSI 的 Icon 表里提取图标时不认 PNG 压缩的
# 条目，结果是「设置 → 应用」里那条记录的 DisplayIcon 根本不会被写入。小尺寸用 DIB
# 兼容性最好，128/256 再用 PNG 以免文件过大。
function Get-DibBytes {
    param([System.Drawing.Bitmap]$Bitmap)

    $w = $Bitmap.Width
    $h = $Bitmap.Height
    $rect = New-Object System.Drawing.Rectangle(0, 0, $w, $h)
    $locked = $Bitmap.LockBits($rect,
        [System.Drawing.Imaging.ImageLockMode]::ReadOnly,
        [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $stride = $locked.Stride
    $pixels = New-Object byte[] ($stride * $h)
    [System.Runtime.InteropServices.Marshal]::Copy($locked.Scan0, $pixels, 0, $pixels.Length)
    $Bitmap.UnlockBits($locked)

    $ms = New-Object System.IO.MemoryStream
    $bw = New-Object System.IO.BinaryWriter($ms)

    # BITMAPINFOHEADER。高度要写成实际高度的两倍（XOR 图 + AND 掩码各占一份）
    $bw.Write([uint32]40)
    $bw.Write([int32]$w)
    $bw.Write([int32]($h * 2))
    $bw.Write([uint16]1)             # planes
    $bw.Write([uint16]32)            # bits per pixel
    $bw.Write([uint32]0)             # BI_RGB，不压缩
    $bw.Write([uint32]($w * $h * 4)) # 图像字节数
    $bw.Write([int32]0); $bw.Write([int32]0)
    $bw.Write([uint32]0); $bw.Write([uint32]0)

    # XOR 位图：DIB 是自下而上存的
    for ($y = $h - 1; $y -ge 0; $y--) {
        $bw.Write($pixels, $y * $stride, $w * 4)
    }

    # AND 掩码：32 位带 alpha 时全填 0 即可，透明度由 alpha 通道决定。
    # 每行按 4 字节对齐。
    $maskStride = [int]([Math]::Floor(($w + 31) / 32) * 4)
    $bw.Write((New-Object byte[] ($maskStride * $h)))

    $bw.Flush()
    $bytes = $ms.ToArray()
    $bw.Dispose(); $ms.Dispose()
    return , $bytes
}

$sizes = @(16, 20, 24, 32, 48, 64, 128, 256)
$images = @()
foreach ($sz in $sizes) {
    $bmp = New-IconBitmap -Size $sz
    if ($sz -le 64) {
        $images += , (Get-DibBytes -Bitmap $bmp)
    } else {
        $images += , (Get-PngBytes -Bitmap $bmp)
    }
    if ($sz -eq 256) {
        $bmp.Save((Join-Path $AssetsDir 'icon-256.png'), [System.Drawing.Imaging.ImageFormat]::Png)
    }
    $bmp.Dispose()
}

# ---- 组装 .ico ----
# ICONDIR: reserved(2)=0, type(2)=1, count(2)
# ICONDIRENTRY x N (每个 16 字节), 然后依次是各尺寸的图像数据
$out = New-Object System.IO.MemoryStream
$bw = New-Object System.IO.BinaryWriter($out)
$bw.Write([uint16]0)
$bw.Write([uint16]1)
$bw.Write([uint16]$sizes.Count)

$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $sz = $sizes[$i]
    $data = $images[$i]
    # 宽/高字段是单字节，256 要写成 0
    $dim = if ($sz -ge 256) { 0 } else { $sz }
    $bw.Write([byte]$dim)          # width
    $bw.Write([byte]$dim)          # height
    $bw.Write([byte]0)             # 调色板颜色数（真彩色写 0）
    $bw.Write([byte]0)             # reserved
    $bw.Write([uint16]1)           # color planes
    $bw.Write([uint16]32)          # bits per pixel
    $bw.Write([uint32]$data.Length)
    $bw.Write([uint32]$offset)
    $offset += $data.Length
}
foreach ($data in $images) { $bw.Write($data) }
$bw.Flush()

$icoPath = Join-Path $AssetsDir 'icon.ico'
[System.IO.File]::WriteAllBytes($icoPath, $out.ToArray())
$bw.Dispose(); $out.Dispose()

Write-Host "生成: $icoPath ($((Get-Item $icoPath).Length) 字节, $($sizes.Count) 个尺寸)" -ForegroundColor Green
Write-Host "生成: $(Join-Path $AssetsDir 'icon-256.png')" -ForegroundColor Green
