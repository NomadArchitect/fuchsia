// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package testsharder

import (
	"fmt"
	"os"
	"path/filepath"
	"testing"

	"github.com/google/go-cmp/cmp"
	"github.com/google/go-cmp/cmp/cmpopts"

	"go.fuchsia.dev/fuchsia/tools/build"
	fintpb "go.fuchsia.dev/fuchsia/tools/integration/fint/proto"
	"go.fuchsia.dev/fuchsia/tools/integration/testsharder/metadata"
	"go.fuchsia.dev/fuchsia/tools/lib/jsonutil"
)

const (
	// The name of the blobs directory within a package repository.
	blobsDirName = "blobs"
)

// Note that just printing a list of shard pointers will print a list of memory addresses,
// which would make for an unhelpful error message.
func assertEqual(t *testing.T, expected, actual []*Shard) {
	t.Helper()
	opts := cmp.Options{
		// We don't care about ordering of shards.
		cmpopts.SortSlices(func(s1, s2 *Shard) bool {
			return s1.Name < s2.Name
		}),
		cmp.FilterValues(func(s1, s2 fintpb.SetArtifacts_Metadata) bool {
			return true
		}, cmp.Ignore()),
	}
	if diff := cmp.Diff(expected, actual, opts...); diff != "" {
		t.Fatalf("shards mismatch: (-want + got):\n%s", diff)
	}
}

func fullTestName(id int, os string) string {
	if os == "fuchsia" {
		return fmt.Sprintf("fuchsia-pkg://fuchsia.com/test%d", id)
	}
	return fmt.Sprintf("/path/to/test%d", id)
}

func makeTest(id int, os string) Test {
	return Test{
		Test: build.Test{
			Name:       fullTestName(id, os),
			PackageURL: fullTestName(id, "fuchsia"),
			Path:       fullTestName(id, "linux"),
			OS:         os,
			Label:      "//src/sys:foo_test(//build/toolchain/fuchsia:x64)",
		},
		Runs: 1,
	}
}

func makeTestWithMetadata(id int, os string, testMetadata metadata.TestMetadata) Test {
	test := makeTest(id, os)
	test.Metadata = testMetadata
	return test
}

func spec(id int, envs ...build.Environment) build.TestSpec {
	return build.TestSpec{
		Test:       makeTest(id, "fuchsia").Test,
		Envs:       envs,
		ExpectsSSH: true,
	}
}

func fuchsiaShard(env build.Environment, ids ...int) *Shard {
	return shard(env, "fuchsia", ids...)
}

func fuchsiaShardWithMetadata(env build.Environment, testMetadata metadata.TestMetadata, ids ...int) *Shard {
	return shardWithMetadata(env, "fuchsia", testMetadata, ids...)
}

func shard(env build.Environment, os string, ids ...int) *Shard {
	var tests []Test
	for _, id := range ids {
		tests = append(tests, makeTest(id, os))
	}
	return &Shard{
		Name:       environmentName(env),
		Tests:      tests,
		Env:        env,
		ExpectsSSH: true,
	}
}

func shardWithMetadata(env build.Environment, os string, testMetadata metadata.TestMetadata, ids ...int) *Shard {
	var tests []Test
	for _, id := range ids {
		tests = append(tests, makeTestWithMetadata(id, os, testMetadata))
	}
	return &Shard{
		Name:       environmentName(env),
		Tests:      tests,
		Env:        env,
		ExpectsSSH: true,
	}
}

func TestMakeShards(t *testing.T) {
	env1 := build.Environment{
		Dimensions: build.DimensionSet{"device_type": "QEMU"},
		Tags:       []string{},
	}
	env2 := build.Environment{
		Dimensions: build.DimensionSet{"device_type": "NUC"},
		Tags:       []string{},
	}
	env3 := build.Environment{
		Dimensions: build.DimensionSet{"os": "Linux"},
		Tags:       []string{},
	}
	env4 := build.Environment{
		Dimensions: build.DimensionSet{"device_type": "AEMU"},
		Tags:       []string{},
	}

	basicOpts := &ShardOptions{
		Tags: []string{},
	}

	t.Run("environments have nonempty names", func(t *testing.T) {
		envs := []build.Environment{env1, env2, env3}
		for _, env := range envs {
			if environmentName(env) == "" {
				t.Fatalf("build.Environment\n%+v\n has an empty name", env)
			}
		}
	})

	t.Run("tests of same environment are grouped", func(t *testing.T) {
		actual := MakeShards(
			[]build.TestSpec{spec(1, env1, env2), spec(2, env1, env3), spec(3, env3)},
			nil,
			basicOpts,
			make(map[string]metadata.TestMetadata),
		)
		expected := []*Shard{fuchsiaShard(env1, 1, 2), fuchsiaShard(env2, 1), fuchsiaShard(env3, 2, 3)}
		assertEqual(t, expected, actual)
	})

	t.Run("metadata is added to tests", func(t *testing.T) {
		testMetadata := metadata.TestMetadata{
			Owners:      []string{"carverforbes@google.com"},
			ComponentID: 1478143,
		}
		metadataMap := make(map[string]metadata.TestMetadata)
		metadataMap["fuchsia-pkg://fuchsia.com/test4"] = testMetadata
		actual := MakeShards(
			[]build.TestSpec{spec(1, env1, env2), spec(2, env1, env3), spec(3, env3), spec(4, env4)},
			nil,
			basicOpts,
			metadataMap,
		)
		expected := []*Shard{fuchsiaShard(env1, 1, 2), fuchsiaShard(env2, 1), fuchsiaShard(env3, 2, 3), fuchsiaShardWithMetadata(env4, testMetadata, 4)}
		assertEqual(t, expected, actual)
	})

	t.Run("there is no deduplication of tests", func(t *testing.T) {
		actual := MakeShards(
			[]build.TestSpec{spec(1, env1), spec(1, env1), spec(1, env1)},
			nil,
			basicOpts,
			make(map[string]metadata.TestMetadata),
		)
		expected := []*Shard{fuchsiaShard(env1, 1, 1, 1)}
		assertEqual(t, expected, actual)
	})

	// Ensure that the order of the shards is the order in which their
	// corresponding environments appear in the input. This is the simplest
	// deterministic order we can produce for the shards.
	t.Run("shards are ordered", func(t *testing.T) {
		actual := MakeShards(
			[]build.TestSpec{spec(1, env2, env3), spec(2, env1), spec(3, env3)},
			nil,
			basicOpts,
			make(map[string]metadata.TestMetadata),
		)
		expected := []*Shard{fuchsiaShard(env2, 1), fuchsiaShard(env3, 1, 3), fuchsiaShard(env1, 2)}
		assertEqual(t, expected, actual)
	})

	t.Run("tags are respected", func(t *testing.T) {
		tagger := func(env build.Environment, tags ...string) build.Environment {
			env2 := env
			env2.Tags = tags
			return env2
		}

		actual := MakeShards(
			[]build.TestSpec{
				spec(1, tagger(env1, "A")),
				spec(2, tagger(env1, "A", "B", "C")),
				spec(3, tagger(env2, "B", "C")),
				spec(4, tagger(env3, "C", "A")),
				spec(5, tagger(env3, "A", "C")),
			},
			nil,
			&ShardOptions{
				Tags: []string{"A", "C"},
			},
			make(map[string]metadata.TestMetadata),
		)
		expected := []*Shard{
			// "C", "A" and "A", "C" should define the same tags.
			fuchsiaShard(tagger(env3, "A", "C"), 4, 5),
		}
		assertEqual(t, expected, actual)
	})

	t.Run("different service accounts get different shards", func(t *testing.T) {
		withAcct := func(env build.Environment, acct string) build.Environment {
			env2 := env
			env2.ServiceAccount = acct
			return env2
		}

		actual := MakeShards(
			[]build.TestSpec{
				spec(1, env1),
				spec(1, withAcct(env1, "acct1")),
				spec(1, withAcct(env1, "acct2")),
			},
			nil,
			basicOpts,
			make(map[string]metadata.TestMetadata),
		)
		expected := []*Shard{
			fuchsiaShard(env1, 1),
			fuchsiaShard(withAcct(env1, "acct1"), 1),
			fuchsiaShard(withAcct(env1, "acct2"), 1),
		}
		assertEqual(t, expected, actual)
	})

	t.Run("netboot envs get different shards", func(t *testing.T) {
		withNetboot := func(env build.Environment) build.Environment {
			env2 := env
			env2.Netboot = true
			return env2
		}

		actual := MakeShards(
			[]build.TestSpec{
				spec(1, env1),
				spec(1, withNetboot(env1)),
			},
			nil,
			basicOpts,
			make(map[string]metadata.TestMetadata),
		)
		expected := []*Shard{
			fuchsiaShard(env1, 1),
			fuchsiaShard(withNetboot(env1), 1),
		}
		assertEqual(t, expected, actual)
	})

	t.Run("isolated tests are in separate shards", func(t *testing.T) {
		isolate := func(test build.TestSpec) build.TestSpec {
			test.Test.Isolated = true
			test.ExpectsSSH = false
			return test
		}

		actual := MakeShards(
			[]build.TestSpec{
				isolate(spec(1, env1, env2)),
				spec(2, env1),
				spec(3, env1),
				isolate(spec(4, env2)),
			},
			nil,
			basicOpts,
			make(map[string]metadata.TestMetadata),
		)

		isolateShard := func(shard *Shard, index int) *Shard {
			shard.Name = fmt.Sprintf("%s-%s", shard.Name, normalizeTestName(shard.Tests[0].Test.Name))
			for i := range shard.Tests {
				shard.Tests[i].Test.Isolated = true
			}
			shard.ExpectsSSH = false
			return shard
		}
		expected := []*Shard{
			isolateShard(fuchsiaShard(env1, 1), 1),
			isolateShard(fuchsiaShard(env2, 1), 1),
			fuchsiaShard(env1, 2, 3),
			isolateShard(fuchsiaShard(env2, 4), 2),
		}
		assertEqual(t, expected, actual)
	})

	t.Run("values from test-list.json are copied over", func(t *testing.T) {
		testListEntry := build.TestListEntry{
			Name: fullTestName(1, "fuchsia"),
			Tags: []build.TestTag{
				{
					Key:   "key",
					Value: "value",
				},
			},
			Execution: build.ExecutionDef{Realm: "/some/realm"},
		}
		actual := MakeShards(
			[]build.TestSpec{
				spec(1, env1, env2),
				spec(2, env1),
				spec(3, env2),
			},
			map[string]build.TestListEntry{
				fullTestName(1, "fuchsia"): testListEntry,
			},
			basicOpts,
			make(map[string]metadata.TestMetadata),
		)

		makeTestWithTagsAndRealm := func(id int, tags []build.TestTag, realm string) Test {
			test := makeTest(id, "fuchsia")
			test.Tags = tags
			test.Realm = realm
			return test
		}
		expected := []*Shard{
			{
				Name:       environmentName(env1),
				Tests:      []Test{makeTestWithTagsAndRealm(1, testListEntry.Tags, "/some/realm"), makeTest(2, "fuchsia")},
				Env:        env1,
				ExpectsSSH: true,
			}, {
				Name:       environmentName(env2),
				Tests:      []Test{makeTestWithTagsAndRealm(1, testListEntry.Tags, "/some/realm"), makeTest(3, "fuchsia")},
				Env:        env2,
				ExpectsSSH: true,
			},
		}
		assertEqual(t, expected, actual)
	})

	t.Run("shard with package repo", func(t *testing.T) {
		buildDir := t.TempDir()
		var blobMerkle, indirectBlobMerkle build.MerkleRoot
		for i := 0; i < 32; i++ {
			blobMerkle[i] = byte(1)
			indirectBlobMerkle[i] = byte(2)
		}
		withPackageManifest := func(test build.TestSpec) build.TestSpec {
			packageManifestPath := fmt.Sprintf("path/to/%s/package_manifest.json", test.Name)
			test.PackageManifests = []string{packageManifestPath}
			absPath := filepath.Join(buildDir, packageManifestPath)
			if err := os.MkdirAll(filepath.Dir(absPath), 0o700); err != nil {
				t.Fatal(err)
			}

			// Write inner subpackage
			subpackageName := test.Name + "subpackage"
			subpackageManifestPath := fmt.Sprintf("path/to/%s/package_manifest.json", subpackageName)
			subAbsPath := filepath.Join(buildDir, subpackageManifestPath)
			if err := os.MkdirAll(filepath.Dir(subAbsPath), 0o700); err != nil {
				t.Fatal(err)
			}
			subpackageManifest := build.PackageManifest{
				Version: "1",
				Blobs: []build.PackageBlobInfo{
					{Merkle: indirectBlobMerkle},
				},
			}
			if err := jsonutil.WriteToFile(subAbsPath, subpackageManifest); err != nil {
				t.Fatal(err)
			}

			packageManifest := build.PackageManifest{
				Version: "1",
				Blobs: []build.PackageBlobInfo{
					{Merkle: blobMerkle},
				},
				Subpackages: []build.PackageSubpackageInfo{
					{Name: subpackageName, ManifestPath: subpackageManifestPath},
				},
			}
			if err := jsonutil.WriteToFile(absPath, packageManifest); err != nil {
				t.Fatal(err)
			}
			return test
		}

		actual := MakeShards(
			[]build.TestSpec{
				withPackageManifest(spec(1, env1, env2)),
				spec(2, env1),
				spec(3, env1),
			},
			nil,
			basicOpts,
			make(map[string]metadata.TestMetadata),
		)

		// Create regular blob.
		writeBlob := func(blobsDir string, blobMerkle build.MerkleRoot) {
			if err := os.MkdirAll(blobsDir, 0o700); err != nil {
				t.Fatal(err)
			}
			if err := os.WriteFile(filepath.Join(blobsDir, blobMerkle.String()), []byte("blob"), 0o700); err != nil {
				t.Fatal(err)
			}
		}
		writeBlob(filepath.Join(buildDir, blobsDirName), blobMerkle)
		writeBlob(filepath.Join(buildDir, blobsDirName), indirectBlobMerkle)
		for _, s := range actual {
			if err := s.CreatePackageRepo(buildDir, "", true); err != nil {
				t.Fatal(err)
			}
			if _, err := os.Stat(filepath.Join(buildDir, s.PkgRepo, blobsDirName, blobMerkle.String())); err != nil {
				t.Error(err)
			}
		}

		makeTestWithPackageManifest := func(id int) Test {
			test := makeTest(id, "fuchsia")
			test.PackageManifests = []string{fmt.Sprintf("path/to/%s/package_manifest.json", test.Name)}
			return test
		}
		expected := []*Shard{
			{
				Name:       environmentName(env1),
				Tests:      []Test{makeTestWithPackageManifest(1), makeTest(2, "fuchsia"), makeTest(3, "fuchsia")},
				Env:        env1,
				ExpectsSSH: true,
				PkgRepo:    fmt.Sprintf("repo_%s", environmentName(env1)),
				Deps:       []string{fmt.Sprintf("repo_%s", environmentName(env1))},
			}, {
				Name:       environmentName(env2),
				Tests:      []Test{makeTestWithPackageManifest(1)},
				Env:        env2,
				ExpectsSSH: true,
				PkgRepo:    fmt.Sprintf("repo_%s", environmentName(env2)),
				Deps:       []string{fmt.Sprintf("repo_%s", environmentName(env2))},
			},
		}
		assertEqual(t, expected, actual)

		// Create delivery blobs.
		deliveryBlobConfig := build.DeliveryBlobConfig{
			Type: 1,
		}
		if err := jsonutil.WriteToFile(filepath.Join(buildDir, deliveryBlobConfigName), deliveryBlobConfig); err != nil {
			t.Fatal(err)
		}
		writeBlob(filepath.Join(buildDir, blobsDirName, "1"), blobMerkle)
		writeBlob(filepath.Join(buildDir, blobsDirName, "1"), indirectBlobMerkle)
		for _, s := range actual {
			if err := s.CreatePackageRepo(buildDir, "", true); err != nil {
				t.Fatal(err)
			}
			// Check that delivery blobs are used.
			if _, err := os.Stat(filepath.Join(buildDir, s.PkgRepo, blobsDirName, "1", blobMerkle.String())); err != nil {
				t.Error(err)
			}
			if _, err := os.Stat(filepath.Join(buildDir, s.PkgRepo, blobsDirName, blobMerkle.String())); !os.IsNotExist(err) {
				t.Errorf("got err: %s; want file not exist err", err)
			}
			// Check that the subpackage blob was included too
			if _, err := os.Stat(filepath.Join(buildDir, s.PkgRepo, blobsDirName, "1", indirectBlobMerkle.String())); err != nil {
				t.Error(err)
			}
			if _, err := os.Stat(filepath.Join(buildDir, s.PkgRepo, blobsDirName, indirectBlobMerkle.String())); !os.IsNotExist(err) {
				t.Errorf("got err: %s; want file not exist err", err)
			}
		}
		// The PkgRepo dirname should be the same so no change is expected in the shards.
		assertEqual(t, expected, actual)
	})
}

func TestMakeShardNamesUnique(t *testing.T) {
	shards := []*Shard{
		{
			Name: "foo",
			Env: build.Environment{
				Dimensions: build.DimensionSet{
					"same_key_and_value": "1",
				},
			},
		},
		{
			Name: "foo",
			Env: build.Environment{
				Dimensions: build.DimensionSet{
					"same_key_and_value": "1",
					"same_key":           "1",
					"other_key":          "blah",
				},
			},
		},
		{
			Name: "foo",
			Env: build.Environment{
				Dimensions: build.DimensionSet{
					"same_key_and_value": "1",
					"same_key":           "2",
				},
			},
		},
		{
			// Should not be updated.
			Name: "bar",
			Env: build.Environment{
				Dimensions: build.DimensionSet{
					"a": "1",
					"b": "1",
				},
			},
		},
	}
	makeShardNamesUnique(shards)

	wantNames := []string{
		"foo",
		"foo-other_key:blah-same_key:1",
		"foo-same_key:2",
		"bar",
	}

	var gotNames []string
	for _, shard := range shards {
		gotNames = append(gotNames, shard.Name)
	}

	if diff := cmp.Diff(wantNames, gotNames); diff != "" {
		t.Errorf("Wrong shard names (-want +got):\n%s", diff)
	}
}
