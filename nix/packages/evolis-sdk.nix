{
  lib,
  fetchurl,
  patchelf,
  autoPatchelfHook,
  python3,
  systemd,
  gcc-unwrapped,
}:
let
  # The ctypes .so has its own dynamic deps (libudev.so.1, libstdc++.so.6)
  # that nothing on the plain Nix store RPath provides. Simply propagating
  # systemd/gcc as propagatedBuildInputs does NOT help: buildPythonPackage
  # only folds *Python modules* into PYTHONPATH, and a ctypes-loaded .so
  # gets no RPath and no wrapper LD_LIBRARY_PATH from a transitive native
  # dep. The fix is an RPath (patchelf) on the .so itself, so it needs no
  # LD_LIBRARY_PATH anywhere — runnable from any environment.
  sdkLibs = lib.makeLibraryPath [
    systemd           # libudev.so.1
    gcc-unwrapped.lib # libstdc++.so.6
  ];
in
# The Evolis SDK ships as a fully self-contained wheel (no Python deps): the
# per-platform native lib is inside the wheel and loaded via ctypes. Format
# "wheel" avoids a pointless source build.
python3.pkgs.buildPythonPackage {
  pname = "evolis-sdk";
  version = "9.4.1";
  src = fetchurl {
    url = "https://files.pythonhosted.org/packages/19/02/b4eca9226cdd1ca1b89f0d9c674e0856ab5d411f5ecf251f4c9c63d79bb0/evolis_sdk-9.4.1-py3-none-any.whl";
    sha256 = "sha256-kii6yXTdGWCb2i6SPgqbEv+P3+0G6kXzTA7ujKCZnw4=";
  };
  format = "wheel";
  # Keep the native libs as buildInputs so they are in the build env's store
  # at fixup time AND as a convenience for tooling that inspects the
  # derivation. The RPath below is what actually makes them load.
  nativeBuildInputs = [autoPatchelfHook];
  buildInputs = [
    systemd
    gcc-unwrapped.lib
  ];
  # Patch every bundled libevolis.so (one per platform is shipped; patching
  # only the one for this system is enough, autopatchelf handles whichever it
  # finds) with the RPath to its runtime deps. autoPatchelfHook also catches
  # the wheel if its .so needs anything beyond the two known libs.
  # The wheel carries no tests; skip the import smoke test too, because the
  # ctypes lib probes hardware state that a sandboxed build cannot.
  doCheck = false;
  dontUsePythonImportsCheck = true;
  preFixup = ''
    find "$out" -name '*.so' -print -exec \
      ${patchelf}/bin/patchelf --set-rpath '${sdkLibs}' {} \;
  '';
  meta = {
    description = "Evolis card printer SDK (self-contained wheel)";
    platforms = lib.platforms.linux;
  };
}
