;; A module importing a name the host does not provide.
;;
;; The host builds its import object from a fixed table, so an unknown name is simply absent
;; and instantiation must fail with a LinkError. That is the desired shape: the failure lands
;; at link time, before any of the module runs, rather than as a trap at an arbitrary later
;; point. It is also how a pref-gated member will behave once guards exist — the entry is not
;; defined at all rather than defined and failing.

(module
  (import "servo:dom/core" "no-such-import" (func $missing))
)
