<#
.SYNOPSIS
Builds DataReco's test disk images inside a Linux container.

.DESCRIPTION
Needs Docker Desktop running. The images and manifest.json land in testdata/out.
Extra arguments go to make_images.py, for example:

    .\testdata\build-images.ps1 --only fat32 ntfs
#>
$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot

docker build --tag datareco-testimages (Join-Path $PSScriptRoot "docker")
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# --privileged is needed to use loop devices and mount the images inside the container.
docker run --rm --privileged --volume "${repo}:/work" datareco-testimages @args
exit $LASTEXITCODE
