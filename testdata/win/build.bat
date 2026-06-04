@echo off
rem Build the Windows DbgEng test fixture (test_target.exe + test_target.pdb).
rem
rem Run from a Visual Studio "x64 Native Tools Command Prompt" (so cl/link + INCLUDE/LIB are set),
rem or call this from a shell that has already run vcvars64.bat. Output lands next to this script.
rem
rem   /Zi  full debug info (emits test_target.pdb)
rem   /Od  no optimization (clean, inspectable locals)
rem   /MT  static CRT (no runtime DLL dependency for the fixture)
rem
rem The .exe/.pdb are build artifacts and are git-ignored; the tests build (or skip when the
rem fixture is absent), so committing the binaries is unnecessary.

setlocal
set HERE=%~dp0
cl /nologo /Zi /Od /MT /Fe:"%HERE%test_target.exe" /Fo:"%HERE%test_target.obj" /Fd:"%HERE%test_target.pdb" "%HERE%test_target.c"
set RC=%ERRORLEVEL%
del "%HERE%test_target.obj" 2>nul
exit /b %RC%
