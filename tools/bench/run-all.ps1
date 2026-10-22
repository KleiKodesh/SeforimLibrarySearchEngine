# Windows wrapper: runs run-all.sh in Git Bash with the same arguments.
#   powershell -ExecutionPolicy Bypass -File tools\bench\run-all.ps1 --steps detect
$bash = @("$env:ProgramFiles\Git\bin\bash.exe", "${env:ProgramFiles(x86)}\Git\bin\bash.exe") | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $bash) { Write-Error "Git Bash not found: install Git for Windows (winget install -e --id Git.Git)"; exit 1 }
& $bash "$PSScriptRoot/run-all.sh" @args
exit $LASTEXITCODE
