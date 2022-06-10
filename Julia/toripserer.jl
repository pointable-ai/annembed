# Some Julia utilities to link exports to Julia Ripserer package

using Logging
using Base.CoreLogging
using Printf

using SparseArrays
using Ripserer
using PersistenceDiagrams
using Plots

logger = ConsoleLogger(stdout, Base.CoreLogging.Debug)
global_logger(logger)


"""
This function reads a matrix in the triplet form i j val and returns
a CSC SparseArrays
"""
function toripserer(fname)
    io = open(fname)
    I = Vector{Int64}()
    J = Vector{Int64}()
    V = Vector{Float64}()
    numline = 0
    for line in eachline(io)
        numline += 1
        sp = split(line, " ")
        if length(sp) != 3 
            @debug line, numline
        end
        # check we have 3 str
        # we add 1 to indexes to got 1 based indexation (we come from Rust)
        push!(I, 1+parse(Int64, sp[1]))
        push!(J, 1+parse(Int64, sp[2]))
        push!(V, parse(Float64, sp[3]))
    end
    cscmat = sparse(I,J,V)
    close(io)
    return cscmat
end


"""
    This function reloads the dump of Rust annembed::fromhnsw::kgproj
"""
function PersistencyAnalyze(fname)
    cscmat = toripserer(fname)

    pers = ripserer(cscmat, dim_max = 3)
    Plots.plot(pers, markersize = 2)
end