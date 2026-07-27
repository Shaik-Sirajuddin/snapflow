package daemon

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
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

	src := filepath.Join(t.TempDir(), "src-proj")
	if err := os.MkdirAll(filepath.Join(src, "clips"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(src, "project.mlt"), []byte("<mlt/>"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(src, "clips", "a.mp4"), []byte("x"), 0o644); err != nil {
		t.Fatal(err)
	}
	srcProj, err := d.ProjectCreate(ctx, ProjectCreateParams{Path: src})
	if err != nil {
		t.Fatalf("create src: %v", err)
	}
	_ = srcProj

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
	raw, err := d.ForwardSAP(ctx, "s-path", sink, "project.open", mustJSON(t, map[string]any{"path": root}))
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
	if _, err := d.ForwardSAP(ctx, "s-path", sink, "project.close", mustJSON(t, map[string]any{})); err != nil {
		t.Fatalf("project.close: %v", err)
	}
}
