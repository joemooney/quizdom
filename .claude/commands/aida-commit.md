<!-- AIDA Generated: v2.0.0 | checksum:09d8e410 | DO NOT EDIT DIRECTLY -->
<!-- To customize: copy this file and modify the copy -->

# AIDA Commit

Commit staged changes with automatic requirement linking.

## Usage

```
/aida-commit [message]
```

## Instructions

Follow the workflow in `.claude/skills/aida-commit.md`:

1. Analyze staged changes and extract requirement traces
2. Check for untraced implementation code
3. Offer to create requirements for untraced work
4. Create commit with requirement links in message
5. Update linked requirement statuses