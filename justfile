binary_name := "nix-installer"

release ONE_PASSWORD_ACCOUNT: (checksum ONE_PASSWORD_ACCOUNT)
	cat dist/SHA256SUMS

clean: 
	rm -rf ./dist
	rm -rf ./target

build: clean
	cargo +nightly-2024-03-12 bin cargo-zigbuild --release --target universal2-apple-darwin

sign-binary $ONE_PASSWORD_ACCOUNT: build
	./bin/sign-binary target/universal2-apple-darwin/release/{{binary_name}} dist/{{binary_name}}

checksum ONE_PASSWORD_ACCOUNT: (sign-binary ONE_PASSWORD_ACCOUNT)
	shasum -a 256 dist/{{binary_name}} > dist/SHA256SUMS
