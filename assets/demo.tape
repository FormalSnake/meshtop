# vhs assets/demo.tape, from the repo root after `cargo build --release`.
# Records the live mesh, so the numbers are whatever the hosts are doing.
Output assets/demo.gif
Set Shell zsh
Set FontSize 14
Set Width 1440
Set Height 860
Set Framerate 20
Set Theme "Catppuccin Mocha"

Hide
Type "./target/release/meshtop" Enter
Sleep 45s
Show

Sleep 3s
Type "2"
Sleep 5s
Right
Sleep 3s
Right
Sleep 3s
Up
Up
Sleep 2s
Type "++"
Sleep 3s
Type "--"
Down
Down
Sleep 2s
Type "3"
Sleep 4s
Type "q"
