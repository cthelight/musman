#! /bin/bash

export CML_VERBOSE="false"
export CML_FORMAT_ORDER=""
export CML_SOURCE_DIR=$(pwd)
export CML_MAX_QUAL_DIR="$(pwd)/max_qual"
export CML_INDIVIDUAL_FORMAT_BASE_DIR="$(pwd)/formats"

while [[ ${1:0:1} == "-" ]]; do
    case ${1:1} in

    "f")
        # Add the next format to the end of the order
        CML_FORMAT_ORDER+=" $2"
        # Considered arg (and the next), move on 2x
        shift
        shift
        ;;
    "v")
        CML_VERBOSE="true"
        # Considered arg, move on
        shift
        ;;
    "s")
        # Set dir where all sources are
        CML_SOURCE_DIR=$2
        # Considered arg (and the next), move on 2x
        shift
        shift
        ;;
    "m")
        # Set the max qual output location
        CML_MAX_QUAL_DIR="$2"
        # Considered arg (and the next), move on 2x
        shift
        shift
        ;;
    "i")
        # Set the base dir for the individual formats
        CML_INDIVIDUAL_FORMAT_BASE_DIR="$2"
        shift
        shift
        ;;
    *)
        # Do Nothing in default case
        ;;

    esac
done

cml_handle_single_format () {
    echo "$1"
    CML_PATH_RELATIVE_SOURCE=$(realpath --relative-to="$CML_SOURCE_DIR" "$1")
    CML_PATH_RELATIVE_NO_EXT=${CML_PATH_RELATIVE_SOURCE%.*}
    CML_FILE_BASENAME=$(basename -- "$1")
    CML_FILE_EXT="${CML_FILE_BASENAME##*.}"

    CML_LINK_PATH="$CML_INDIVIDUAL_FORMAT_BASE_DIR/$CML_FILE_EXT/$CML_PATH_RELATIVE_SOURCE"

    mkdir -p "$(dirname "$CML_LINK_PATH")"
    ln -f "$1" "$CML_LINK_PATH"

    # Now we look at the max quality stuff

    BETTER_FMTS="true"
    for FMT in ${CML_FORMAT_ORDER}
    do
        CML_FMT_LINK="$CML_MAX_QUAL_DIR/$CML_PATH_RELATIVE_NO_EXT.$FMT"
        if [ "$FMT" == "$CML_FILE_EXT" ]; then
            # We have seen all "better" formats, so not better anymore
            BETTER_FMTS="false"
            # Also know we are the "best" so add our link
            mkdir -p "$(dirname "$CML_FMT_LINK")"
            ln -f "$1" "$CML_FMT_LINK"
            continue
        fi
        if [ "$BETTER_FMTS" == "true" ]; then
            # If the same file with the current better format already exists, the work has been done, just return
            if [ -e "$CML_FMT_LINK" ]; then
                return 0
            fi
        else
            # if a worse format exists, delete it
            if [ -e "$CML_FMT_LINK" ]; then
                rm -f "$CML_FMT_LINK"
            fi
        fi
    done
}
export -f cml_handle_single_format

# $1 - File path
# $2 - "container" directory path
cml_check_clean_link() {
    echo "$1"
    CML_PATH_RELATIVE=$(realpath --relative-to="$2" "$1")
    # Check if file is not the same as in source dir (or removed from the source dir)
    # If so, remove the link as well.
    if ! [ "$1" -ef "$CML_SOURCE_DIR/$CML_PATH_RELATIVE" ]; then
        rm -f "$1"
    fi
}

export -f cml_check_clean_link

echo "$CML_FORMAT_ORDER"

export FMT
for FMT in ${CML_FORMAT_ORDER}
do
    # Create all of the links (max quality and format specific) for all files of $FMT type
    find "$CML_SOURCE_DIR" -name "*.$FMT" -exec bash -c 'cml_handle_single_format "{}"' \;
    # Cleans up the individual format directory for current format
    find "$CML_INDIVIDUAL_FORMAT_BASE_DIR/$FMT" -name "*.$FMT" -exec bash -c 'cml_check_clean_link "{}" "$CML_INDIVIDUAL_FORMAT_BASE_DIR/$FMT/"' \;
    find "$CML_INDIVIDUAL_FORMAT_BASE_DIR/$FMT" -type d -empty -delete
done
# Cleanup max quality directory
find "$CML_MAX_QUAL_DIR" -type f -exec bash -c 'cml_check_clean_link "{}" "$CML_MAX_QUAL_DIR/"' \;
find "$CML_MAX_QUAL_DIR" -type d -empty -delete

mkdir "$CML_MAX_QUAL_DIR/.stfolder"


