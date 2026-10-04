# ccc - Code Change Capture

# the extension package `npm run package` writes
vsix := "dist/ccc-codecache.vsix"

# list the recipes
default:
    @just --list

# build the analyser the extension runs - target/release/ccc
build:
    CCC_SKIP_VSIX=1 cargo build --release

# package the vs code extension on its own
package-extension:
    cd extensions/vscode && npm run package

# install the packaged extension over the one vs code has
install-extension:
    code --install-extension {{vsix}} --force
    @echo "now run 'Developer: Reload Window' in VS Code - it loads the new extension and starts the new analyser"

# build the analyser and the extension, then install the extension
reinstall: build package-extension install-extension
