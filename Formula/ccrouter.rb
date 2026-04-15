class Ccrouter < Formula
  desc "Lightweight CLI proxy to route Claude Code to any LLM provider"
  homepage "https://github.com/guo/ccrouter"
  version "0.1.9"
  license "MIT"

  on_macos do
    url "https://github.com/guo/ccrouter/releases/download/v#{version}/ccrouter-v#{version}-universal-apple-darwin.tar.gz"
    sha256 "95775ba1247aaed6054318b43e27a6decf00a8a6dc82496880c647a87fc0c43e"
  end

  on_linux do
    on_intel do
      url "https://github.com/guo/ccrouter/releases/download/v#{version}/ccrouter-v#{version}-x86_64-unknown-linux-musl.tar.gz"
      sha256 "9952c94a3167212e7d041c7a6fc6b166e1344f4c585d6694b7fb6c888462ce8e"
    end
    on_arm do
      url "https://github.com/guo/ccrouter/releases/download/v#{version}/ccrouter-v#{version}-aarch64-unknown-linux-musl.tar.gz"
      sha256 "dcd59d91c6a533b55bf131c40428d57b753ad773ba8f7513f97c1709ef62a44a"
    end
  end

  def install
    bin.install "ccrouter"
  end

  test do
    assert_match "ccrouter", shell_output("#{bin}/ccrouter --version")
  end
end
