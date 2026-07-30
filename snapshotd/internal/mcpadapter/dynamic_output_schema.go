package mcpadapter

import (
	"encoding/json"
	"fmt"
	"reflect"

	"github.com/google/jsonschema-go/jsonschema"
	"github.com/mark3labs/mcp-go/mcp"
)

// anyType is reflect.TypeFor[any](), used as the ForOptions.TypeSchemas key
// below.
var anyType = reflect.TypeOf((*any)(nil)).Elem()

// permissiveDynamicSchema is the schema substituted for every `any`-typed
// field (Clip.Source, PlaylistEntry.Source, PlaylistEntryDetail.Source,
// FilterListEntry.Properties, KeyframeInfo.Value -- see output_types.go's
// package doc) when building a dynamicOutputSchema. jsonschema-go's default
// translation of `any` is an empty Schema{}, which marshals to the JSON
// Schema boolean literal `true` ("anything goes", valid per the 2020-12
// spec) -- but that boolean form isn't an object, and at least one real MCP
// client's outputSchema validator rejects a non-object value at
// properties.<field>, which fails that tool's registration outright rather
// than just being laxer than intended. An explicit multi-type schema means
// the same "anything goes" semantics without ever being a bare boolean.
var permissiveDynamicSchema = &jsonschema.Schema{
	Types: []string{"object", "string", "number", "boolean", "array", "null"},
}

// dynamicOutputSchema is a drop-in replacement for mcp.WithOutputSchema[T]()
// for any T that (transitively) contains an `any`-typed field, per
// permissiveDynamicSchema's doc above. It reflects T the same way
// WithOutputSchema does (including forcing the top-level type to "object",
// per the MCP output-schema spec) but substitutes permissiveDynamicSchema
// for `any` fields before marshaling, then hands the result to
// mcp.WithRawOutputSchema.
func dynamicOutputSchema[T any]() mcp.ToolOption {
	schema, err := jsonschema.For[T](&jsonschema.ForOptions{
		IgnoreInvalidTypes: true,
		TypeSchemas: map[reflect.Type]*jsonschema.Schema{
			anyType: permissiveDynamicSchema,
		},
	})
	if err != nil {
		// T is one of this package's own output_types.go structs, so a
		// failure here means a coding mistake, not bad runtime input.
		panic(fmt.Errorf("dynamicOutputSchema[%T]: %w", *new(T), err))
	}
	schema.Type = "object"

	b, err := json.Marshal(schema)
	if err != nil {
		panic(fmt.Errorf("dynamicOutputSchema[%T]: marshal: %w", *new(T), err))
	}
	return mcp.WithRawOutputSchema(json.RawMessage(b))
}
