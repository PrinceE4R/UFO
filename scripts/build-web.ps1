# Build Web WASM + static bundle into frontend/pkg
cargo build -p ufo-wasm --target wasm32-unknown-unknown --release
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
New-Item -ItemType Directory -Force -Path "frontend\pkg" | Out-Null
wasm-bindgen --target web --out-dir frontend\pkg target\wasm32-unknown-unknown\release\ufo_wasm.wasm
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Write-Host "Web bundle ready in frontend/"
