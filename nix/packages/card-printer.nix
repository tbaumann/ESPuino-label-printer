{
  lib,
  python3,
  evolisSdk,
}:
# The local print spooler: our Python package, with the Evolis SDK as a required
# dependency (it is not optional — talking to the printer is the spooler's whole
# job). buildPythonApplication productionises the console entrypoint into a
# wrapper that puts the SDK's propagated native libs (libudev, libstdc++) on
# LD_LIBRARY_PATH automatically, so the runnable `card-printer` needs nothing
# extra.
python3.pkgs.buildPythonApplication {
  pname = "card-printer";
  version = "0.1.0";
  src = lib.fileset.toSource {
    root = ../../printer;
    fileset = ../../printer;
  };
  format = "pyproject";
  propagatedBuildInputs = [evolisSdk];
  nativeBuildInputs = [python3.pkgs.setuptools python3.pkgs.wheel];
  build-system = [python3.pkgs.setuptools];
  doCheck = false;
  meta = {
    description = "ESPuino Label Printer spooler (local Evolis card printer)";
    platforms = lib.platforms.linux;
  };
}
