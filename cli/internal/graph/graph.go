// Package graph contains the CompleteGraph struct and some methods around it
package graph

import (
	gocontext "context"
	"fmt"

	"github.com/pyr-sh/dag"
	"github.com/vercel/turbo/cli/internal/fs"
	"github.com/vercel/turbo/cli/internal/nodes"
	"github.com/vercel/turbo/cli/internal/turbopath"
	"github.com/vercel/turbo/cli/internal/util"
)

// WorkspaceInfos holds information about each workspace in the monorepo.
type WorkspaceInfos map[string]*fs.PackageJSON

// CompleteGraph represents the common state inferred from the filesystem and pipeline.
// It is not intended to include information specific to a particular run.
type CompleteGraph struct {
	// WorkspaceGraph expresses the dependencies between packages
	WorkspaceGraph dag.AcyclicGraph

	// Pipeline is config from turbo.json
	Pipeline fs.Pipeline

	// WorkspaceInfos stores the package.json contents by package name
	WorkspaceInfos WorkspaceInfos

	// GlobalHash is the hash of all global dependencies
	GlobalHash string

	RootNode string
}

// GetPackageTaskVisitor wraps a `visitor` function that is used for walking the TaskGraph
// during execution (or dry-runs). The function returned here does not execute any tasks itself,
// but it helps curry some data from the Complete Graph and pass it into the visitor function.
func (g *CompleteGraph) GetPackageTaskVisitor(ctx gocontext.Context, visitor func(ctx gocontext.Context, packageTask *nodes.PackageTask) error) func(taskID string) error {
	return func(taskID string) error {
		packageName, taskName := util.GetPackageTaskFromId(taskID)

		pkg, ok := g.WorkspaceInfos[packageName]
		if !ok {
			return fmt.Errorf("cannot find package %v for task %v", packageName, taskID)
		}

		packageTask := &nodes.PackageTask{
			TaskID:      taskID,
			Task:        taskName,
			PackageName: packageName,
			Pkg:         pkg,
		}

		taskDefinition, err := g.getResolvedTaskDefinition(pkg, taskID, taskName)
		if err != nil {
			return err
		}

		packageTask.TaskDefinition = taskDefinition

		return visitor(ctx, packageTask)
	}
}

// getResolvedTaskDefinition currently just looks for the definition in the Pipeline
// defined in the Graph. Later, this will get Pipelines defined in the task's workspace as well.
func (g *CompleteGraph) getResolvedTaskDefinition(pkg *fs.PackageJSON, taskID string, taskName string) (*fs.TaskDefinition, error) {
	return g.getMergedTaskDefinition(pkg, taskID, taskName)
}

func getTaskFromPipeline(pipeline fs.Pipeline, taskID string, taskName string) (*fs.TaskDefinition, error) {
	// first check for package-tasks
	taskDefinition, ok := pipeline[taskID]
	if !ok {
		// then check for regular tasks
		fallbackTaskDefinition, notcool := pipeline[taskName]
		// if neither, then bail
		if !notcool {
			// Return an empty fs.TaskDefinition
			return nil, fmt.Errorf("No task defined in pipeline")
		}

		// override if we need to...
		taskDefinition = fallbackTaskDefinition
	}

	return &taskDefinition, nil
}

func (g *CompleteGraph) getMergedTaskDefinition(pkg *fs.PackageJSON, taskID string, taskName string) (*fs.TaskDefinition, error) {
	taskDefinitions, err := g.getTaskDefinitionChain(pkg, taskID, taskName)
	if err != nil {
		return nil, err
	}

	// reverse the array, because we want to start with the end of the chain.
	for i, j := 0, len(taskDefinitions)-1; i < j; i, j = i+1, j-1 {
		taskDefinitions[i], taskDefinitions[j] = taskDefinitions[j], taskDefinitions[i]
	}

	// Start with an empty definition
	mergedTaskDefinition := &fs.TaskDefinition{}

	// For each of the TaskDefinitions we know of, merge them in
	for _, taskDef := range taskDefinitions {
		mergedTaskDefinition.Outputs = taskDef.Outputs
		mergedTaskDefinition.ShouldCache = taskDef.ShouldCache
		mergedTaskDefinition.EnvVarDependencies = taskDef.EnvVarDependencies
		mergedTaskDefinition.TopologicalDependencies = taskDef.TopologicalDependencies
		mergedTaskDefinition.TaskDependencies = taskDef.TaskDependencies
		mergedTaskDefinition.Inputs = taskDef.Inputs
		mergedTaskDefinition.OutputMode = taskDef.OutputMode
		mergedTaskDefinition.Persistent = taskDef.Persistent
	}

	return mergedTaskDefinition, nil
}

func (g *CompleteGraph) getTaskDefinitionChain(pkg *fs.PackageJSON, taskID string, taskName string) ([]fs.TaskDefinition, error) {
	// Start a list of TaskDefinitions we've found for this TaskID
	taskDefinitions := []fs.TaskDefinition{}

	// Start in the workspace directory
	turboJSONPath := turbopath.AbsoluteSystemPath(pkg.Dir).UntypedJoin("turbo.json")
	_, err := fs.ReadTurboConfig(turboJSONPath)

	// If there is no turbo.json in the workspace directory, we'll use the one in root and carry on.
	if err != nil {
		rootTaskDefinition, _ := getTaskFromPipeline(g.Pipeline, taskID, taskName)
		taskDefinitions = append(taskDefinitions, *rootTaskDefinition)
		return taskDefinitions, nil
	}

	// For loop until we `break` manually.
	// We will reassign `turboJSONPath` inside this loop, so that
	// every time we iterate, we're starting from a new one.
	for {
		turboJSON, err := fs.ReadTurboConfig(turboJSONPath)
		if err != nil {
			return nil, err
		}

		// TODO(mehulkar):
		// 		getTaskFromPipeline allows searching with a taskID (e.g. `package#task`).
		// 		But we do not want to allow this, except if we're in the root workspace.
		taskDefinition, err := getTaskFromPipeline(turboJSON.Pipeline, taskID, taskName)
		if err != nil {
			// If there was nothing in the pipeline for this task
			// We can exit
			break
		} else {
			// Add it into the taskDefinitions
			taskDefinitions = append(taskDefinitions, *taskDefinition)

			// If this turboJSON doesn't have an extends property, we can stop our for loop here.
			if len(turboJSON.Extends) == 0 {
				break
			}

			// TODO(mehulkar): Enable extending from more than one workspace.
			// TODO(mehulkar): Enable extending from non-root workspace.
			if len(turboJSON.Extends) > 1 || turboJSON.Extends[0] != util.RootPkgName {
				return nil, fmt.Errorf(
					"You can only extend from the root workspace. \"%s\" extends from %v",
					pkg.Name,
					turboJSON.Extends,
				)
			}

			// If there's an extends property, walk up to the next one, find the workspace it refers to,
			// and and assign `directory` to it for the next iteration in this for loop.
			// Note(mehulkar):
			//		We are looping through all items in Extends, but as of now,
			// 		and based on the checks above, we only want to read the first item
			// 		(and we already know that it's the root workspace).
			for _, workspaceName := range turboJSON.Extends {
				workspace, ok := g.WorkspaceInfos[workspaceName]
				if !ok {
					// TODO: Should this be a hard error?
					// A workspace was referenced that doesn't exist or we know nothing about
					break
				}

				// Reassign these. The loop will run again with this new turbo.json now.
				turboJSONPath = turbopath.AbsoluteSystemPath(workspace.Dir).UntypedJoin("turbo.json")
			}
		}
	}

	return taskDefinitions, nil
}
