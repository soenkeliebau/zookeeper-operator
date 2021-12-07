{ nixpkgs ? builtins.fetchTarball {
    # Latest commit on nixpkgs-unstable branch
    url = https://github.com/NixOS/nixpkgs/archive/cdaa4ce25b74ed006b110e2a635eade0e3f18980.tar.gz;
    sha256 = "19bsfmlvz30rrjdxkwhdl0svyhl6yvk26z9dxljw2gl77s89b099";
  }
, pkgs ? import nixpkgs {}
, cargo ? import ./Cargo.nix {
    inherit nixpkgs pkgs;
    defaultCrateOverrides = pkgs.defaultCrateOverrides // {
    };
  }
}:
rec {
  build = cargo.workspaceMembers.stackable-zookeeper-operator.build;
  docker = pkgs.dockerTools.streamLayeredImage {
    name = "stackable-zookeeper-operator";
    tag = "latest";
    config = {
      Cmd = [ (build+"/bin/stackable-zookeeper-operator") "run" ];
    };
  };
}
