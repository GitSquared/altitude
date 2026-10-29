class Altitude < Formula
  desc "Terminal control plane for delegated coding agents"
  homepage "https://github.com/GitSquared/altitude"
  url "https://github.com/GitSquared/altitude/releases/download/v0.5.0/altitude-0.5.0-aarch64-apple-darwin.tar.gz"
  sha256 "202bdb3eb737fe4fa1bff509deb6090c488c0e597a2ff39e6ac710d408425cbb"
  license "BUSL-1.1"
  version "0.5.0"

  depends_on :macos
  depends_on arch: :arm64

  def install
    bin.install "altitude-0.5.0-aarch64-apple-darwin/altitude"
  end

  test do
    assert_match "altitude v#{version}", shell_output("#{bin}/altitude --version")
  end
end
