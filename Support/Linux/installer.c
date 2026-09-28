/* Branded Linux installer. Define ALIGN_PACKAGE_ARCH for Omarchy/Arch. */
#include <gtk/gtk.h>

static GtkWidget *window, *status_label, *detail_label, *button, *progress;
static char *package_path, *icon_path;
static gboolean installing = FALSE, installed = FALSE;
static guint pulse_id = 0;

static gboolean pulse(gpointer unused) {
    (void)unused;
    gtk_progress_bar_pulse(GTK_PROGRESS_BAR(progress));
    return G_SOURCE_CONTINUE;
}

static gboolean close_request(GtkWindow *w, gpointer unused) {
    (void)w; (void)unused;
    return installing;
}

static void finished(GObject *object, GAsyncResult *result, gpointer unused) {
    (void)unused;
    GError *error = NULL;
    char *out = NULL, *err = NULL;
    gboolean communicated = g_subprocess_communicate_utf8_finish(G_SUBPROCESS(object), result, &out, &err, &error);
    installing = FALSE;
    if (pulse_id) { g_source_remove(pulse_id); pulse_id = 0; }
    gtk_widget_set_sensitive(button, TRUE);
    if (communicated && g_subprocess_get_successful(G_SUBPROCESS(object))) {
        installed = TRUE;
        gtk_progress_bar_set_fraction(GTK_PROGRESS_BAR(progress), 1.0);
        gtk_label_set_text(GTK_LABEL(status_label), "You're ready to sync.");
        gtk_label_set_text(GTK_LABEL(detail_label), "Align is installed. Find it in your applications, or open it below.");
        gtk_button_set_label(GTK_BUTTON(button), "Open Align");
    } else {
        gtk_progress_bar_set_fraction(GTK_PROGRESS_BAR(progress), 0);
        gtk_label_set_text(GTK_LABEL(status_label), "Installation wasn't completed.");
        const char *reason = error ? error->message : (err && *err ? err : out);
        gtk_label_set_text(GTK_LABEL(detail_label), reason && *reason ? reason : "Authorization was cancelled. You can try again.");
        gtk_button_set_label(GTK_BUTTON(button), "Try again");
    }
    g_clear_error(&error);
    g_free(out); g_free(err);
    g_object_unref(object);
}

static void install_clicked(GtkButton *b, gpointer unused) {
    (void)b; (void)unused;
    GError *error = NULL;
    if (installed) {
        GSubprocess *app = g_subprocess_new(G_SUBPROCESS_FLAGS_NONE, &error, "/opt/align/align", NULL);
        if (app) { g_object_unref(app); gtk_window_destroy(GTK_WINDOW(window)); }
        else {
            gtk_label_set_text(GTK_LABEL(detail_label), error->message);
            g_clear_error(&error);
        }
        return;
    }
    // Only the package manager is elevated. UI and Align run as the user.
    GSubprocess *process = g_subprocess_new(
        G_SUBPROCESS_FLAGS_STDOUT_PIPE | G_SUBPROCESS_FLAGS_STDERR_PIPE, &error,
#ifdef ALIGN_PACKAGE_ARCH
        "/usr/bin/pkexec", "/usr/bin/pacman", "-U", "--noconfirm", package_path, NULL);
#else
        "/usr/bin/pkexec", "/usr/bin/dnf", "--assumeyes", "install", package_path, NULL);
#endif
    if (!process) {
        gtk_label_set_text(GTK_LABEL(detail_label), error->message);
        g_clear_error(&error);
        return;
    }
    installing = TRUE;
    gtk_widget_set_sensitive(button, FALSE);
    gtk_button_set_label(GTK_BUTTON(button), "Installing…");
    gtk_label_set_text(GTK_LABEL(status_label), "Installing Align…");
    gtk_label_set_text(GTK_LABEL(detail_label), "Approve the system prompt. Required dependencies will be installed automatically.");
    pulse_id = g_timeout_add(90, pulse, NULL);
    g_subprocess_communicate_utf8_async(process, NULL, NULL, finished, NULL);
}

static GtkWidget *label(const char *text, const char *css) {
    GtkWidget *widget = gtk_label_new(text);
    gtk_label_set_xalign(GTK_LABEL(widget), 0);
    gtk_widget_add_css_class(widget, css);
    return widget;
}

static void activate(GtkApplication *app, gpointer unused) {
    (void)unused;
    GtkCssProvider *css = gtk_css_provider_new();
    gtk_css_provider_load_from_string(css,
        "window { background: #18191b; color: #f2f2f7; }"
        ".brand { font-size: 30px; font-weight: 700; }"
        ".eyebrow { font-size: 10px; letter-spacing: 1px; color: #ababb4; }"
        ".headline { font-size: 32px; font-weight: 600; }"
        ".subtitle { font-size: 16px; color: #ababb4; }"
        ".status { font-size: 15px; font-weight: 600; }"
        ".detail { font-size: 13px; color: #ababb4; }"
        ".footnote { font-size: 12px; color: #ababb4; }"
        "button.install { background: #0a84ff; color: white; border: 0; border-radius: 10px;"
        " padding: 12px 26px; font-weight: 600; box-shadow: none; }"
        "button.install:hover { background: #409cff; }"
        "button.install:disabled { background: #2b2d31; color: #ababb4; }"
        "progressbar trough { background: #2b2d31; border: 0; min-height: 5px; border-radius: 4px; }"
        "progressbar progress { background: #0a84ff; border: 0; min-height: 5px; border-radius: 4px; }"
        ".lane { min-height: 13px; border-radius: 4px; }"
        ".blue { background: #0a84ff; } .cyan { background: #32ade6; } .green { background: #30d158; }");
    gtk_style_context_add_provider_for_display(gdk_display_get_default(), GTK_STYLE_PROVIDER(css), GTK_STYLE_PROVIDER_PRIORITY_APPLICATION);
    g_object_unref(css);
    window = gtk_application_window_new(app);
    gtk_window_set_title(GTK_WINDOW(window), "Install Align");
    gtk_window_set_default_size(GTK_WINDOW(window), 720, 520);
    gtk_window_set_resizable(GTK_WINDOW(window), FALSE);
    g_signal_connect(window, "close-request", G_CALLBACK(close_request), NULL);
    GtkWidget *box = gtk_box_new(GTK_ORIENTATION_VERTICAL, 0);
    gtk_widget_set_margin_start(box, 48); gtk_widget_set_margin_end(box, 48);
    gtk_widget_set_margin_top(box, 36); gtk_widget_set_margin_bottom(box, 32);
    gtk_window_set_child(GTK_WINDOW(window), box);
    GtkWidget *brand = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 18);
    GtkWidget *icon = gtk_image_new_from_file(icon_path);
    gtk_image_set_pixel_size(GTK_IMAGE(icon), 64);
    gtk_box_append(GTK_BOX(brand), icon);
    GtkWidget *name = gtk_box_new(GTK_ORIENTATION_VERTICAL, 4);
    gtk_widget_set_valign(name, GTK_ALIGN_CENTER);
    gtk_box_append(GTK_BOX(name), label("Align", "brand"));
    gtk_box_append(GTK_BOX(name), label("AUDIO & VIDEO SYNC", "eyebrow"));
    gtk_box_append(GTK_BOX(brand), name); gtk_box_append(GTK_BOX(box), brand);
    GtkWidget *title = label("Everything in sync.", "headline");
    gtk_widget_set_margin_top(title, 30); gtk_box_append(GTK_BOX(box), title);
    GtkWidget *subtitle = label("Your recordings. One timeline.", "subtitle");
    gtk_widget_set_margin_top(subtitle, 8); gtk_box_append(GTK_BOX(box), subtitle);
    const char *colors[] = {"blue", "cyan", "green"};
    for (int i = 0; i < 3; i++) {
        GtkWidget *lane = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 0);
        gtk_widget_add_css_class(lane, "lane"); gtk_widget_add_css_class(lane, colors[i]);
        gtk_widget_set_margin_top(lane, i == 0 ? 30 : 7);
        gtk_widget_set_size_request(lane, 500 - i * 55, 13);
        gtk_widget_set_halign(lane, GTK_ALIGN_START); gtk_box_append(GTK_BOX(box), lane);
    }
    status_label = label("A little setup. Then you're in sync.", "status");
    gtk_widget_set_margin_top(status_label, 28); gtk_box_append(GTK_BOX(box), status_label);
    detail_label = label("Includes the app, media tools, AAF support and command-line tools.", "detail");
    gtk_widget_set_margin_top(detail_label, 8);
    gtk_label_set_wrap(GTK_LABEL(detail_label), TRUE);
    gtk_label_set_max_width_chars(GTK_LABEL(detail_label), 76);
    gtk_label_set_lines(GTK_LABEL(detail_label), 3);
    gtk_label_set_ellipsize(GTK_LABEL(detail_label), PANGO_ELLIPSIZE_END);
    gtk_box_append(GTK_BOX(box), detail_label);
    progress = gtk_progress_bar_new();
    gtk_widget_set_margin_top(progress, 24); gtk_box_append(GTK_BOX(box), progress);
    GtkWidget *footer = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 16);
    gtk_widget_set_margin_top(footer, 24);
    GtkWidget *note = label("Free. Open source. Yours.", "footnote");
    gtk_widget_set_hexpand(note, TRUE); gtk_box_append(GTK_BOX(footer), note);
    button = gtk_button_new_with_label("Install Align");
    gtk_widget_add_css_class(button, "install");
    g_signal_connect(button, "clicked", G_CALLBACK(install_clicked), NULL);
    gtk_box_append(GTK_BOX(footer), button); gtk_box_append(GTK_BOX(box), footer);
    gtk_window_present(GTK_WINDOW(window));
}

int main(int argc, char **argv) {
    if (argc != 3 || !g_file_test(argv[1], G_FILE_TEST_IS_REGULAR)) {
        g_printerr("Usage: align-installer PACKAGE ICON.png\n"); return 2;
    }
    package_path = g_canonicalize_filename(argv[1], NULL);
    icon_path = g_canonicalize_filename(argv[2], NULL);
    GtkApplication *app = gtk_application_new("com.align.installer", G_APPLICATION_NON_UNIQUE);
    g_signal_connect(app, "activate", G_CALLBACK(activate), NULL);
    int status = g_application_run(G_APPLICATION(app), 1, argv);
    g_object_unref(app); g_free(package_path); g_free(icon_path);
    return status;
}
