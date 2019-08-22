.PHONY: test build install

test:
	racket test/unit-tests.rkt

build:
	cd futil; raco make main.rkt
	@echo "done"

install:
	cd futil; raco pkg install

uninstall:
	raco pkg remove futil