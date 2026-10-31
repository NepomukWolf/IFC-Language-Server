;;; ifc-mode.el --- Major mode for IFC STEP files -*- lexical-binding: t; -*-

;; Package-Requires: ((emacs "29.1"))

;;; Commentary:

;; Highlighting for ISO 10303-21 (STEP) files holding IFC models.
;; Strings and comments come from the syntax table; every font-lock regex
;; matches a single token, so highlighting cost does not grow with line
;; length or file size.

;;; Code:

(defvar ifc-mode-syntax-table
  (let ((table (make-syntax-table)))
    (modify-syntax-entry ?' "\"" table)
    (modify-syntax-entry ?\" "." table)
    (modify-syntax-entry ?\\ "." table)
    (modify-syntax-entry ?/ ". 14" table)
    (modify-syntax-entry ?* ". 23" table)
    (modify-syntax-entry ?_ "_" table)
    (dolist (c '(?= ?$ ?- ?+ ?& ?| ?< ?> ?%))
      (modify-syntax-entry c "." table))
    table)
  "Syntax table for `ifc-mode'.")

(defvar ifc-font-lock-keywords
  '(("\\_<\\(?:END-\\)?ISO-10303-21\\_>\\|\\_<\\(?:HEADER\\|DATA\\|ENDSEC\\)\\_>"
     . 'font-lock-keyword-face)
    ("\\_<\\(?:FILE_DESCRIPTION\\|FILE_NAME\\|FILE_SCHEMA\\)\\_>"
     . 'font-lock-builtin-face)
    ("#[0-9]+" . 'font-lock-variable-name-face)
    ("\\_<IFC[A-Z0-9_]*\\_>" . 'font-lock-type-face)
    ("\\.[A-Z_][A-Z0-9_]*\\." . 'font-lock-constant-face)
    ("[-+]?\\_<[0-9]+\\(?:\\.[0-9]*\\)?\\(?:[Ee][-+]?[0-9]+\\)?"
     . 'font-lock-number-face))
  "Font-lock keywords for `ifc-mode'.")

;;;###autoload
(define-derived-mode ifc-mode fundamental-mode "IFC"
  "Major mode for ISO 10303-21 IFC files."
  (setq-local font-lock-defaults '(ifc-font-lock-keywords))
  (setq-local comment-start "/* ")
  (setq-local comment-end " */"))

;;;###autoload
(add-to-list 'auto-mode-alist '("\\.[iI][fF][cC]\\'" . ifc-mode))

(provide 'ifc-mode)
;;; ifc-mode.el ends here
