# plan
I am a constant tinkerer. Between work, personal projects, and a vibrant homelab there is a long list of things I do on the computer. This tool is inspired by [a post](https://garbagecollected.org/2017/10/24/the-carmack-plan/) on John Carmac’s plan files.

Importantly this isn’t a faithful replication of the plan files but some things I blatantly stole are:
* a plain text format
* the imperative bullet as the main unit 
* support for prose

The goal is to make it as easy as possible to drop a line in the plan files from anywhere in the terminal while taking the tedium out of managing files.

The tool supports command line injection into a header section called the inbox and even supports piping from other commands.

Of course, the files are just text and you can open and modify them as you see fit. Go write in it.

## Neovim
There’s a neovim plugin that adds syntax highlighting and support for minimal toggling of TODO type entries. It also supports markdown syntax highlighting for portions of the file.