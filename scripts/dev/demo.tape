# budi demo — records assets/demo.gif
# Prerequisites: symlink /tmp/demo-projects/flask → your Flask repo
# Run from budi repo root: vhs scripts/dev/demo.tape

Output assets/demo.gif

Require budi

Set Shell "zsh"
Set FontSize 14
Set Width 1000
Set Height 620
Set Padding 20
Set Theme "Catppuccin Mocha"
Set TypingSpeed 30ms
Set PlaybackSpeed 1
Set WindowBar Colorful
Set BorderRadius 8
Set Framerate 24

# Start in a project
Type "cd /tmp/demo-projects/flask"
Enter
Sleep 500ms

# Step 1: Init and index
Type "budi init --index"
Enter
Sleep 8s

# Step 2: Preview — the "aha" moment
Type `budi repo preview "Where is full_dispatch_request defined?"`
Enter
Sleep 8s

# Pause to read the output
Sleep 4s
