/* Synthetic native owner for the isolated contract test. Never a stock CLI. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/wait.h>

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "--version") == 0) {
        puts("codex-cli 0.156.1");
        return 0;
    }
    int offset = argc == 5 && strcmp(argv[1], "--remote=synthetic") == 0 ? 1 : 0;
    if (argc != 4 + offset) return 2;
    pid_t child = fork();
    if (child == 0) {
        execl(argv[1 + offset], argv[1 + offset], argv[2 + offset], argv[3 + offset], (char *)NULL);
        _exit(127);
    }
    if (child < 0) return 3;
    int status;
    if (waitpid(child, &status, 0) < 0) return 4;
    return WIFEXITED(status) ? WEXITSTATUS(status) : 5;
}
