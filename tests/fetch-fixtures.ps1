# Test files are real PSTs, so they are not committed. These three are the public
# fixtures from hrbrmstr/freepst: a PST, an OST, and a "password-protected" PST that
# reads exactly like the other two.
$dir = Join-Path $PSScriptRoot 'data'
New-Item -ItemType Directory -Force $dir | Out-Null
foreach ($f in 'dist-list.pst', 'example-2013.ost', 'passworded.pst') {
    Invoke-WebRequest "https://github.com/hrbrmstr/freepst/raw/master/inst/extdata/$f" -OutFile (Join-Path $dir $f)
}
Get-ChildItem $dir | Select-Object Name, Length
