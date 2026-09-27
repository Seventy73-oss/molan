@echo off
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul 2>&1
cd /d [LOCAL_PATH]\Documents\dsh-work\dsh\molan-work\rust
cargo test -p molan-core --lib facts
