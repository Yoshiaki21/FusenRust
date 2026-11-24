# Windows 版の配布用 zip を作る（docs/decisions.md D-011）。
#
# 使い方（リポジトリの直下で）:
#   powershell -ExecutionPolicy Bypass -File scripts\package-windows.ps1
#
# target\dist\FusenRust-<版>-windows-x64.zip ができる。中身は fusen-desktop.exe と README.txt だけで、
# 好きな場所に展開して fusen-desktop.exe を実行すればよい（インストール不要）。

$ErrorActionPreference = 'Stop'
Set-Location (Split-Path -Parent $PSScriptRoot)

cargo build --release -p fusen-desktop
if ($LASTEXITCODE -ne 0) { throw 'ビルドに失敗しました' }

$version = (Select-String -Path 'crates\fusen-desktop\Cargo.toml' -Pattern '^version\s*=\s*"([^"]+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
$name = "FusenRust-$version-windows-x64"
$stage = Join-Path 'target\dist' $name
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force $stage | Out-Null

Copy-Item 'target\release\fusen-desktop.exe' $stage
@'
FusenRust（Windows 版）

fusen-desktop.exe を実行すると起動します（トレイにアイコンが出ます）。
  fusen-desktop.exe --new        新しい付箋を作る（ショートカットキーに割り当てると便利です）
  fusen-desktop.exe --show-all   全付箋を一時的に前面に出す / 戻す
  fusen-desktop.exe --manager    管理画面を開く
  fusen-desktop.exe --quit       終了する

データ:  %APPDATA%\fusen-rust\           （fusen.db と blobs\）
設定:    %APPDATA%\fusen-rust\settings.json
ログ:    %LOCALAPPDATA%\fusen-rust\fusen-desktop.log
自動起動は管理画面の設定で切り替えます（HKCU\...\Run に登録します）。
'@ | Set-Content -Encoding utf8 (Join-Path $stage 'README.txt')

$zip = "target\dist\$name.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path "$stage\*" -DestinationPath $zip
Write-Host "作成しました: $zip"
