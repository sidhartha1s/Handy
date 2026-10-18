@echo off
rem Tests and builds fix/gpu-retry-backoff on the ZBook, in the logged-in session so the GPU tests see the RTX A2000.
set L=%USERPROFILE%\bench-dictation\results\gpu-retry.log
set W=%USERPROFILE%\Automation Projects\Handy\.worktrees\gpu-retry
set CARGO_TARGET_DIR=%USERPROFILE%\Automation Projects\Handy\src-tauri\target
echo start %DATE% %TIME% > "%L%"
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" > nul 2>&1
cargo test --lib --manifest-path "%W%\src-tauri\Cargo.toml" >> "%L%" 2>&1
echo unit exit %ERRORLEVEL% >> "%L%"
cargo build --manifest-path "%W%\src-tauri\Cargo.toml" >> "%L%" 2>&1
echo debug build exit %ERRORLEVEL% >> "%L%"
set HANDY_TRANSCRIBE_WORKER_EXE=%CARGO_TARGET_DIR%\debug\handy.exe
set HANDY_TEST_MODEL=%USERPROFILE%\.cache\huggingface\hub\models--handy-computer--parakeet-unified-en-0.6b-gguf\snapshots\7e948f21b7bdbac698d3318db9d350f1096f3b6c\parakeet-unified-en-0.6b-Q8_0.gguf
set HANDY_TEST_WAV=%USERPROFILE%\AppData\Roaming\com.pais.handy\recordings\handy-1792280872.wav
cargo test --lib --manifest-path "%W%\src-tauri\Cargo.toml" engine_supervisor -- --ignored --nocapture --test-threads=1 >> "%L%" 2>&1
echo ignored exit %ERRORLEVEL% >> "%L%"
pushd "%W%"
call bun install >> "%L%" 2>&1
call bun run tauri build --no-bundle >> "%L%" 2>&1
echo release build exit %ERRORLEVEL% >> "%L%"
git checkout -- src-tauri/Cargo.toml
popd
echo done %DATE% %TIME% >> "%L%"
