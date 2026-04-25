# plan

I am a constant tinkerer. Between work, personal projects, and a vibrant homelab,
there is a long list of things I do on the computer.

`plan` is a small command-line tool for maintaining daily plaintext plan files.
It is inspired by [John Carmack's plan files][carmack-plan], but it is not a
faithful recreation.

The pieces I stole are:

- a plain text format
- imperative bullets as the main unit of activity
- room for prose

The goal is to make it as easy as possible to drop a line into a plan file from
anywhere in the terminal while taking the tedium out of managing files.

`plan` supports command-line injection into a header section called the inbox,
and it can read entries from pipes:

```sh
plan log "ship the thing"
echo "rough note from another command" | plan jot -