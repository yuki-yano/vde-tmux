#include <Python.h>
#include <stdlib.h>
#include <string.h>

int main(int argc, char **argv) {
    if (argc == 2 && !strcmp(argv[1], "--no-daemon") && getenv("CODEX_FIXTURE_SCRIPT")) {
        char *python_args[] = {argv[0], getenv("CODEX_FIXTURE_SCRIPT")};
        return Py_BytesMain(2, python_args);
    }
    return Py_BytesMain(argc, argv);
}
