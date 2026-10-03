# The write `harness.self`'s PostToolUse sensor must catch: the path is
# inside the program, so no pattern over the command line sees it (#1516).
with open("docs/divergence.toml", "a") as registry:
    registry.write("\n# appended by a program the hook's words never named\n")
