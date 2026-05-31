<!-- AIDA Generated: v2.0.0 | checksum:e32c9263 | DO NOT EDIT DIRECTLY -->
<!-- To customize: copy this file and modify the copy -->

# Unified Search

Search across requirements and code simultaneously.

## Usage

Invoke with: `/aida-search <query>`

## Instructions

1. Search requirements: `aida search "$ARGUMENTS"`
2. Search code for trace comments: `grep -r "trace:.*$ARGUMENTS" src/`
3. Correlate results — show which requirements have implementation and vice versa
4. Identify gaps: untraced code and unimplemented requirements