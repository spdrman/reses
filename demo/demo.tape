# The README demo: pick an account, find the mail, save it as the inbox, read a message.
# Run it through demo/record.sh (make demo), which provides the MinIO, the seeded bucket,
# the throwaway credentials and the reses binary this tape expects.

Output demo.gif
Output demo.txt

Require reses

Set Shell bash
Set FontFamily "JetBrains Mono"
Set FontSize 16
Set Width 1120
Set Height 660
Set Padding 16
Set Framerate 12
Set TypingSpeed 80ms
Set CursorBlink false

# xterm.js here answers that it can draw sixel images, so reses would draw the real logo, but
# vhs builds each frame from xterm.js's text layer and the image never reaches the GIF. The
# styled-text wordmark is what a recording can show.
Env RESES_LOGO "text"

# vhs writes a text snapshot after every command, and demo/check-frames.sh checks those. So each
# keystroke is followed by a short Sleep of its own before the pause: that snapshot shows the
# screen just after the change, and the one after the pause shows where it settled.

# The accounts screen.
Type "reses"
Sleep 400ms
Enter
Sleep 300ms
Sleep 2.2s

# Connect, then open the bucket and the folder that holds the mail.
Enter
Sleep 300ms
Sleep 1.2s
Down
Sleep 300ms
Sleep 400ms
Enter
Sleep 300ms
Sleep 1.2s
Enter
Sleep 300ms
# Let the email marks come in.
Sleep 2.7s

# Save it as the inbox.
Type "i"
Sleep 300ms
Sleep 3.2s

# Open a message and scroll through it.
Down
Sleep 300ms
Sleep 500ms
Enter
Sleep 300ms
Sleep 2.2s
Down@250ms 8
Sleep 300ms
Sleep 700ms
PageDown
Sleep 300ms
Sleep 1.7s

# Back to the inbox.
Type "q"
Sleep 300ms
Sleep 2.7s
