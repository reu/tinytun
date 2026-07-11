{
  lib,
  rustPlatform,
}:

rustPlatform.buildRustPackage (finalAttrs: {
  pname = "tinytun";
  version = "0.4.2";

  src = lib.cleanSource ./.;

  env = {
    TINYTUN_DEFAULT_SERVER_URL = "https://tinytun.com:5554";
  };

  cargoBuildFlags = [ "-p" "tinytun-client" ];
  doCheck = false;

  cargoHash = "sha256-PlaR0AejWDrL0tN6aszHy5jI2ZNRJwu+FDa8Zp6F23Q=";

  meta = {
    description = "Expose local web servers on the internet";
    homepage = "https://github.com/reu/tinytun";
    license = lib.licenses.mit;
    mainProgram = "tinytun";
  };
})
