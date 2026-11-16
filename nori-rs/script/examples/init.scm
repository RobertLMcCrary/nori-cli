;; An example $NORI_HOME/init.scm. nori/workflow is already loaded.

;; Land work as small, reviewable commits and a tidy pull request.
(define-workflow land
  (turns 3)
  (on-change "Commit what you just changed as focused commits; explain the why.")
  (on-stop "Run the tests that cover what you changed, and fix what fails.")
  (on-pr "Check that the description lists every user-visible change.")
  (on-ci-fail "Fix what CI reports, push, and say what broke.")
  (commit-style 'conventional #:max-files 8 #:max-subject 72 #:atomic #t
                #:batch 'per-change #:require-body #t)
  (commit-agent 'claude #:model "haiku")
  (pr #:title "<type>(<scope>): <summary>"
      #:sections '("Why" "What" "Testing")
      #:max-lines 400
      #:base "main"
      #:upstream-when 'tests-pass))

;; Look around without committing anything.
(define-workflow explore
  (turns 10)
  (instruct "Do not change files; report what you find.")
  (pr #:upstream-when 'never))

;; The same chain, written with the thread-first macro.
(define hotfix
  (~> (workflow "hotfix")
      (use land)
      (turns 1)
      (hook-before-stop "Say what you did not verify.")
      (pr #:max-lines 50 #:draft #t)))
(register-workflow hotfix)

(default-workflow land)
