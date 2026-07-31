package daemon

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"snapshotd/internal/registry"
)

func TestProjectCreate_PathFirstAndListShape(t *testing.T) {
	bin := buildSapFixture(t)
	d := newSapFixtureDaemon(t, bin)
	ctx := context.Background()

	root := filepath.Join(t.TempDir(), "my-proj")
	proj, err := d.ProjectCreate(ctx, ProjectCreateParams{
		Path:        root,
		ProjectType: registry.ProjectTypeFolder,
	})
	if err != nil {
		t.Fatalf("ProjectCreate: %v", err)
	}
	if proj.RootDir != root {
		t.Fatalf("RootDir=%q want %q", proj.RootDir, root)
	}
	if proj.ProjectType != registry.ProjectTypeFolder {
		t.Fatalf("ProjectType=%q", proj.ProjectType)
	}
	if proj.Path != root {
		t.Fatalf("Path=%q want %q", proj.Path, root)
	}
	if proj.ProjectID != proj.ID {
		t.Fatalf("ProjectID mismatch")
	}
	if _, err := os.Stat(root); err != nil {
		t.Fatalf("folder missing: %v", err)
	}
	// No .mlt until open+save.
	if _, err := os.Stat(filepath.Join(root, proj.MltFileName)); !os.IsNotExist(err) {
		t.Fatalf("expected no mlt yet, err=%v", err)
	}

	list, err := d.ListProjects(ctx)
	if err != nil {
		t.Fatalf("ListProjects: %v", err)
	}
	if len(list) != 1 || list[0].Path == "" || list[0].ProjectID == "" {
		t.Fatalf("list path-first shape: %+v", list)
	}

	// daemon.createProject still works as thin wrapper.
	legacy, err := d.CreateProject(ctx, CreateProjectParams{Name: "legacy-name"})
	if err != nil {
		t.Fatalf("CreateProject: %v", err)
	}
	if legacy.RootDir == "" || legacy.ProjectType != registry.ProjectTypeFolder {
		t.Fatalf("legacy create: %+v", legacy)
	}
}

func TestProjectClone_CopiesTree(t *testing.T) {
	bin := buildSapFixture(t)
	d := newSapFixtureDaemon(t, bin)
	ctx := context.Background()

	// Create first (path must not already exist), then populate media.
	src := filepath.Join(t.TempDir(), "src-proj")
	srcProj, err := d.ProjectCreate(ctx, ProjectCreateParams{Path: src})
	if err != nil {
		t.Fatalf("create src: %v", err)
	}
	if err := os.MkdirAll(filepath.Join(src, "clips"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(src, "project.mlt"), []byte("<mlt/>"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(src, "clips", "a.mp4"), []byte("x"), 0o644); err != nil {
		t.Fatal(err)
	}

	dest := filepath.Join(t.TempDir(), "dest-proj")
	cloned, err := d.ProjectClone(ctx, ProjectCloneParams{SourcePath: src, DestPath: dest})
	if err != nil {
		t.Fatalf("clone: %v", err)
	}
	if cloned.ID == srcProj.ID {
		t.Fatalf("clone must be new id")
	}
	if _, err := os.Stat(filepath.Join(dest, "clips", "a.mp4")); err != nil {
		t.Fatalf("media not copied: %v", err)
	}
	if _, err := os.Stat(filepath.Join(dest, "project.mlt")); err != nil {
		t.Fatalf("mlt not copied: %v", err)
	}
}

func TestProjectCreate_DuplicatePathRejected(t *testing.T) {
	bin := buildSapFixture(t)
	d := newSapFixtureDaemon(t, bin)
	ctx := context.Background()

	root := filepath.Join(t.TempDir(), "dup-proj")
	first, err := d.ProjectCreate(ctx, ProjectCreateParams{Path: root})
	if err != nil {
		t.Fatalf("first create: %v", err)
	}

	// Registry-level: second create same path must fail, not reuse.
	_, err = d.ProjectCreate(ctx, ProjectCreateParams{Path: root})
	if err == nil {
		t.Fatalf("expected ErrProjectAlreadyExists on second create")
	}
	if !errors.Is(err, registry.ErrProjectAlreadyExists) {
		t.Fatalf("want ErrProjectAlreadyExists, got %v", err)
	}
	if !strings.Contains(err.Error(), first.ID) {
		t.Fatalf("error should wrap existing id: %v", err)
	}

	// Filesystem-level: unregistered but existing dir must also fail.
	orphan := filepath.Join(t.TempDir(), "orphan-dir")
	if err := os.MkdirAll(orphan, 0o755); err != nil {
		t.Fatal(err)
	}
	_, err = d.ProjectCreate(ctx, ProjectCreateParams{Path: orphan})
	if !errors.Is(err, registry.ErrProjectAlreadyExists) {
		t.Fatalf("want ErrProjectAlreadyExists for existing unregistered path, got %v", err)
	}
}

func TestProjectList_ActiveMarker(t *testing.T) {
	bin := buildSapFixture(t)
	d := newSapFixtureDaemon(t, bin)
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()

	root := filepath.Join(t.TempDir(), "active-proj")
	proj, err := d.ProjectCreate(ctx, ProjectCreateParams{Path: root})
	if err != nil {
		t.Fatalf("create: %v", err)
	}

	list, err := d.ProjectList(ctx, ProjectListParams{})
	if err != nil {
		t.Fatalf("list: %v", err)
	}
	if len(list) != 1 {
		t.Fatalf("want 1 project, got %d", len(list))
	}
	if list[0].Active || list[0].IsOpen {
		t.Fatalf("expected inactive before open, got active=%v isOpen=%v", list[0].Active, list[0].IsOpen)
	}

	// Open so a ready ProcessInstance is registered.
	sink := &fanoutSink{}
	if _, err := d.ForwardSAP(ctx, "s-active", sink, "project.enter", mustJSON(t, map[string]any{"projectId": proj.ID})); err != nil {
		t.Fatalf("open: %v", err)
	}

	list, err = d.ProjectList(ctx, ProjectListParams{})
	if err != nil {
		t.Fatalf("list after open: %v", err)
	}
	if !list[0].Active || !list[0].IsOpen {
		t.Fatalf("expected active after open, got %+v", list[0])
	}

	// Kill child; default list stays stale-ready; refresh:true should clear.
	instances, err := d.Reg.ListProcessInstancesByProject(proj.ID)
	if err != nil || len(instances) == 0 {
		t.Fatalf("instances: %+v err=%v", instances, err)
	}
	proc, err := os.FindProcess(instances[0].PID)
	if err != nil {
		t.Fatalf("find process: %v", err)
	}
	_ = proc.Kill()
	time.Sleep(200 * time.Millisecond)

	stale, err := d.ProjectList(ctx, ProjectListParams{})
	if err != nil {
		t.Fatalf("stale list: %v", err)
	}
	// DB may still say ready until refresh (eventually consistent).
	_ = stale

	refreshed, err := d.ProjectList(ctx, ProjectListParams{Refresh: true})
	if err != nil {
		t.Fatalf("refresh list: %v", err)
	}
	if refreshed[0].Active || refreshed[0].IsOpen {
		t.Fatalf("refresh should mark dead child inactive, got active=%v isOpen=%v", refreshed[0].Active, refreshed[0].IsOpen)
	}
}

func TestProjectOpen_PathViaForwardSAP(t *testing.T) {
	bin := buildSapFixture(t)
	d := newSapFixtureDaemon(t, bin)
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()

	root := filepath.Join(t.TempDir(), "path-open")
	if err := os.MkdirAll(root, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(root, "project.mlt"), []byte("<mlt/>"), 0o644); err != nil {
		t.Fatal(err)
	}

	sink := &fanoutSink{}
	// Primary name project.open with path.
	raw, err := d.ForwardSAP(ctx, "s-path", sink, "project.enter", mustJSON(t, map[string]any{"path": root}))
	if err != nil {
		t.Fatalf("project.open path: %v", err)
	}
	var st map[string]any
	if err := json.Unmarshal(raw, &st); err != nil {
		t.Fatal(err)
	}
	if st["opened"] != true {
		t.Fatalf("expected opened=true: %+v", st)
	}
	// Close via primary name.
	if _, err := d.ForwardSAP(ctx, "s-path", sink, "project.exit", mustJSON(t, map[string]any{})); err != nil {
		t.Fatalf("project.close: %v", err)
	}
}

func TestProjectOpen_MissingPathDoesNotCreateProject(t *testing.T) {
	bin := buildSapFixture(t)
	d := newSapFixtureDaemon(t, bin)
	ctx := context.Background()

	root := filepath.Join(t.TempDir(), "missing-project")
	before, err := d.ListProjects(ctx)
	if err != nil {
		t.Fatalf("list before: %v", err)
	}

	_, err = d.ForwardSAP(ctx, "s-missing", &fanoutSink{}, "project.enter", mustJSON(t, map[string]any{
		"path": filepath.Join(root, "missing.mlt"),
	}))
	if err == nil || !strings.Contains(err.Error(), "use project.create first") {
		t.Fatalf("project.open missing path error=%v, want explicit create guidance", err)
	}

	after, err := d.ListProjects(ctx)
	if err != nil {
		t.Fatalf("list after: %v", err)
	}
	if len(after) != len(before) {
		t.Fatalf("missing project.open created a registry row: before=%d after=%d", len(before), len(after))
	}
	if _, statErr := os.Stat(root); !os.IsNotExist(statErr) {
		t.Fatalf("missing project.open created filesystem path %s: stat err=%v", root, statErr)
	}
}
