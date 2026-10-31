" Vim syntax file
" Language: IFC / STEP physical file (ISO 10303-21)
" Filenames: *.ifc, and *.stp / *.step / *.p21 starting with ISO-10303-21;
"
" Only regex matches over the visible lines: no multi-line constructs other than strings and
" comments, so redraw cost does not depend on file size.

if exists('b:current_syntax')
  finish
endif

let s:cpo_save = &cpo
set cpo&vim

syn case match

syn match   ifcEntity         "\<\u[A-Z0-9_]*\ze\s*("
syn match   ifcEntity         "=\s*\zs\u[A-Z0-9_]*"
syn keyword ifcHeaderEntity   FILE_DESCRIPTION FILE_NAME FILE_SCHEMA FILE_POPULATION SECTION_LANGUAGE SECTION_CONTEXT SCHEMA_POPULATION
syn match   ifcSection        "^\s*\%(END-\)\=ISO-10303-21\ze\s*;"
syn match   ifcSection        "^\s*\%(HEADER\|DATA\|ENDSEC\)\ze\s*[;(]"

syn match   ifcNumber         "[-+]\=\<\d\+\%(\.\d*\)\=\%([eE][-+]\=\d\+\)\="
syn match   ifcReference      "#\d\+"
syn match   ifcInstanceId     "^\s*#\d\+\ze\s*="
syn match   ifcEnum           "\.\u[A-Z0-9_]*\."
syn match   ifcBoolean        "\.[TFU]\."
syn match   ifcNull           "\$"
syn match   ifcDerived        "\*"
syn match   ifcDelimiter      "[=(),;]"

syn region  ifcString         start=+'+ skip=+''+ end=+'+ contains=ifcStringEscape
syn match   ifcStringEscape   contained "''\|\\\\\|\\[SP]\\.\|\\X\\\x\x\|\\X[24]\\\x*\\X0\\"
syn region  ifcBinary         start=+"+ end=+"+ oneline
syn region  ifcComment        start="/\*" end="\*/" contains=@Spell

" A line that opens an instance (#id=) is never inside a string or comment in practice.
syn sync match ifcSync grouphere NONE "^\s*#\d\+\s*="
syn sync maxlines=200

hi def link ifcSection        PreProc
hi def link ifcHeaderEntity   Keyword
hi def link ifcEntity         Type
hi def link ifcInstanceId     Identifier
hi def link ifcReference      Identifier
hi def link ifcNumber         Number
hi def link ifcEnum           Constant
hi def link ifcBoolean        Boolean
hi def link ifcNull           Special
hi def link ifcDerived        Special
hi def link ifcDelimiter      Delimiter
hi def link ifcString         String
hi def link ifcStringEscape   SpecialChar
hi def link ifcBinary         String
hi def link ifcComment        Comment

let b:current_syntax = 'ifc'

let &cpo = s:cpo_save
unlet s:cpo_save
