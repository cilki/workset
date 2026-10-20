# Bash completions for workset.
#
# Source this file from ~/.bashrc, or copy it into a bash-completion
# directory such as /usr/share/bash-completion/completions/.

# COMP_LINE and COMP_POINT are unexported shell variables during programmable
# completion, so they must be passed to the binary explicitly.
#
# The candidates arrive one per line and are read with `mapfile` rather than
# assigned from an unquoted `$(...)`: a candidate is a repo path, which is a
# directory name the user never typed, and inside an array assignment it would
# be glob-expanded against the current directory. A repo directory named '*'
# therefore replaced itself in the suggestions with every entry of the
# workspace — including bare directory names that are not repos at all but are
# still accepted as patterns, so `workset drop <TAB>` could offer `github.com`,
# which drops every repo under it. `mapfile` performs neither word splitting
# nor pathname expansion, so each line becomes exactly one candidate.
_workset_complete() {
    mapfile -t COMPREPLY < <(
        _ARGCOMPLETE_=bash \
        COMP_LINE="$COMP_LINE" \
        COMP_POINT="$COMP_POINT" \
        COMP_TYPE="$COMP_TYPE" \
        COMP_KEY="$COMP_KEY" \
        "$1" 2>/dev/null
    )
}

complete -o bashdefault -o default -F _workset_complete workset
