;; Default fallback policy, evaluated before user and trusted project scripts.
(define-workflow nori-default (fallback-ranked-models))
(default-workflow nori-default)
