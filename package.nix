{
  lib,
  rustPlatform,
}:

rustPlatform.buildRustPackage {
  pname = "tinytun";
  version = "0.4.0";

  src = lib.cleanSource ./.;

  env = {
    TINYTUN_DEFAULT_SERVER_URL = "https://tinytun.com:5554";
  };

  cargoBuildFlags = [ "-p" "tinytun-client" ];
  doCheck = false;

  cargoHash = "sha256-HQzL06x8+BuSLanReRipUAFiMk9XAlt6ANTF6cK8wpA=";

  meta = {
    description = "Expose local web servers on the internet";
    homepage = "https://github.com/reu/tinytun";
    license = lib.licenses.mit;
    mainProgram = "tinytun";
  };
}
