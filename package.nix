{
  lib,
  rustPlatform,
}:

rustPlatform.buildRustPackage {
  pname = "tinytun";
  version = "0.3.0";

  src = lib.cleanSource ./.;

  cargoBuildFlags = [ "-p" "tinytun-client" ];

  meta = {
    description = "Expose local web servers on the internet";
    homepage = "https://github.com/reu/tinytun";
    license = lib.licenses.mit;
    mainProgram = "tinytun";
  };
}
