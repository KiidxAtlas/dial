class Dial < Formula
  desc "Local-first coding agent harness"
  homepage "https://github.com/KiidxAtlas/dial"
  version "0.1.18"
  on_macos do
    on_arm do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "5c758ff5975451df0de6d501f8901aa0adc63acdc619bb37948b08f615877164"
    end
    on_intel do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "214d96e3937ac211e2ee9df1bb9c65580c2f79a374c0cb2d8f956b1d8cc7d2a2"
    end
  end
  on_linux do
    on_intel do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "0aec1008dba3afc839915d7d00f91b96e92550daf26db776227a1f5b8cf5f348"
    end
  end

  def install
    bin.install "dial"
    share.install "LICENSE", "README.md", "THIRD_PARTY_NOTICES", "Silver.LICENSE", "Unscii.LICENSE"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/dial --version")
  end
end
