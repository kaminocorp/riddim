/*
 * A C program that drives the engine through the C ABI only. Compiled and run by
 * tests/c_abi.rs. Exit code 0 means every step did what the header says; any other code
 * identifies the step that did not.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "riddim.h"

#define CHECK(code, condition)                                                       \
    do {                                                                             \
        if (!(condition)) {                                                          \
            fprintf(stderr, "step %d failed: %s (last error: \"%s\")\n", (code),     \
                    #condition, riddim_last_error());                                \
            return (code);                                                           \
        }                                                                            \
    } while (0)

/* Polls until an event containing `needle` arrives; returns 1 if it did. */
static int wait_for_event(RiddimEngine *engine, const char *needle) {
    for (int attempt = 0; attempt < 2000; attempt++) {
        char *event = riddim_poll_event(engine);
        if (event != NULL) {
            int found = strstr(event, needle) != NULL;
            riddim_string_free(event);
            if (found) {
                return 1;
            }
        } else {
            riddim_sync(engine);
        }
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc < 2) {
        return 1;
    }
    CHECK(10, riddim_abi_version() == RIDDIM_ABI_VERSION);

    /* A bad configuration is refused with a message. */
    CHECK(11, riddim_engine_new("{\"device\":\"nope\",\"capture_dir\":\"/tmp\"}") == NULL);
    CHECK(12, strstr(riddim_last_error(), "unknown device") != NULL);
    CHECK(13, riddim_engine_new(NULL) == NULL);

    char config[2048];
    snprintf(config, sizeof config,
             "{\"device\":\"manual\",\"capture_dir\":\"%s\",\"main_thread\":\"internal\"}", argv[1]);
    RiddimEngine *engine = riddim_engine_new(config);
    CHECK(20, engine != NULL);

    /* Ops that are not JSON are refused before they reach the engine. */
    CHECK(21, riddim_apply(engine, "not json") == 0);
    CHECK(22, strlen(riddim_last_error()) > 0);

    /* A valid batch: one track, and the metronome on so there is something to hear. */
    const char *ops =
        "[{\"op\":\"add_track\",\"index\":0,\"track\":{\"id\":7,\"name\":\"From C\",\"kind\":\"audio\","
        "\"gain\":1.0,\"pan\":0.0,\"mute\":false,\"solo\":false,\"record_armed\":false,"
        "\"monitor\":false,\"input\":{\"type\":\"none\"},\"output\":{\"type\":\"master\"}}},"
        "{\"op\":\"set_metronome\",\"metronome\":{\"enabled\":true,\"gain\":0.5,\"count_in_bars\":0}}]";
    uint64_t ticket = riddim_apply(engine, ops);
    CHECK(30, ticket != 0);
    CHECK(31, wait_for_event(engine, "\"event\":\"applied\""));

    /* A batch the model refuses comes back as a "rejected" event, not a crash. */
    CHECK(32, riddim_apply(engine, "[{\"op\":\"remove_track\",\"id\":99}]") != 0);
    CHECK(33, wait_for_event(engine, "\"event\":\"rejected\""));

    char *model = riddim_model_json(engine);
    CHECK(40, model != NULL && strstr(model, "\"name\":\"From C\"") != NULL);

    RiddimStatus status;
    CHECK(50, riddim_status(engine, &status) == 1);
    CHECK(51, status.transport == RIDDIM_STOPPED && status.sample_rate == 48000);
    CHECK(52, status.model_revision == 1);

    /* Play and render a tenth of a second: the first metronome click is in there. */
    CHECK(60, riddim_transport(engine, RIDDIM_PLAY, 0) == 1);
    CHECK(61, riddim_sync(engine) == 1);
    static float audio[2 * 4800];
    CHECK(62, riddim_run(engine, 4800, audio) == 2);
    CHECK(63, audio[0] == 0.0f && audio[2] != 0.0f && audio[2] == audio[3]);
    CHECK(64, riddim_status(engine, &status) == 1);
    CHECK(65, status.transport == RIDDIM_PLAYING && status.position_samples == 4800);

    /* Locate, and the status follows. */
    CHECK(70, riddim_transport(engine, RIDDIM_LOCATE, 96000) == 1);
    CHECK(71, riddim_sync(engine) == 1);
    CHECK(72, riddim_run(engine, 128, NULL) == 2);
    CHECK(73, riddim_status(engine, &status) == 1 && status.position_samples == 96128);

    /* The model goes out as JSON and comes back in as a whole project. */
    uint64_t revision = riddim_load_model(engine, model);
    CHECK(80, revision > 1);
    riddim_string_free(model);
    CHECK(81, riddim_load_model(engine, "{\"schema\":1}") == 0);
    CHECK(82, strlen(riddim_last_error()) > 0);

    CHECK(90, riddim_capture_plugin_states(engine) == 1);
    CHECK(91, riddim_main_thread_pump(engine) == 1);
    CHECK(92, riddim_set_param(engine, 1, 2, 0.5) == 1);
    CHECK(93, riddim_play_note(engine, 7, 0, 60, 1.0f) == 1);
    CHECK(94, riddim_status(NULL, &status) == 0);

    /* The buffer size changes under a running engine; a size out of range is refused. */
    CHECK(110, riddim_set_buffer_size(engine, 256) == 256);
    CHECK(111, riddim_status(engine, &status) == 1 && status.block_frames == 256);
    CHECK(112, wait_for_event(engine, "\"event\":\"stream_changed\""));
    CHECK(113, riddim_set_buffer_size(engine, 3) == 0);
    CHECK(114, strlen(riddim_last_error()) > 0);
    CHECK(115, status.events_lost == 0);

    /* Touching a fader: valid targets are accepted, nonsense is refused. */
    CHECK(95, riddim_touch(engine, 7, "{\"type\":\"track_gain\"}", 1) == 1);
    CHECK(96, riddim_touch(engine, 7, "{\"type\":\"track_gain\"}", 0) == 1);
    CHECK(97, riddim_touch(engine, 7, "{\"type\":\"volume\"}", 1) == 0);
    CHECK(98, strlen(riddim_last_error()) > 0);

    /* Bounce one second of the project to a file, offline, in an engine of its own. */
    char *project = riddim_model_json(engine);
    CHECK(100, project != NULL);
    char request[1024];
    snprintf(request, sizeof request,
             "{\"path\":\"%s/bounce.wav\",\"media_dir\":\"%s\",\"end\":{\"time\":282240000},"
             "\"sample_rate\":48000,\"block_frames\":256}",
             argv[1], argv[1]);
    char *bounced = riddim_bounce(project, request);
    CHECK(101, bounced != NULL);
    CHECK(102, strstr(bounced, "\"frames\":48000") != NULL);
    CHECK(103, strstr(bounced, "\"missing_plugins\":[]") != NULL);
    riddim_string_free(bounced);
    CHECK(104, riddim_bounce(project, "{\"path\":\"x.wav\"}") == NULL);
    CHECK(105, strstr(riddim_last_error(), "bounce request") != NULL);
    CHECK(106, riddim_bounce(NULL, request) == NULL);
    riddim_string_free(project);

    riddim_engine_free(engine);
    riddim_engine_free(NULL);
    riddim_string_free(NULL);
    printf("c abi smoke test passed\n");
    return 0;
}
